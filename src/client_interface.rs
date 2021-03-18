pub mod trow_proto {
    include!("../lib/protobuf/out/trow.rs");
}

use crate::registry_interface::digest::{Digest as if_digest, DigestAlgorithm};
use crate::registry_interface::{
    CatalogOperations, Metrics, MetricsError, Validation, ValidationError,
};
use tokio::runtime::Runtime;
use trow_proto::{
    admission_controller_client::AdmissionControllerClient, registry_client::RegistryClient,
    BlobRef, CatalogRequest, CompleteRequest, HealthRequest, ListTagsRequest,
    ManifestHistoryRequest, ManifestRef, MetricsRequest, ReadinessRequest, UploadRef,
    UploadRequest, VerifyManifestRequest,
};

use tonic::{Code, Request};

use crate::types::{self, *};
use crate::{
    chrono::TimeZone,
    registry_interface::{BlobStorage, ManifestStorage, StorageDriverError},
};
use failure::Error;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io;
use std::io::prelude::*;
use std::{convert::TryInto, str::FromStr};

// BIG TODO:
// Creating a new runtime for each request is awful.
// Wasn't clear how to manage this in rocket, might need to pass runtime in or something.

pub struct ClientInterface {
    server: String,
}

/**
 * This is really bad way to do things on several levels, but it works for the moment.
 *
 * The major problem is Rust doesn't have TCO so we could be DOS'd by a malicious request.
 */
fn extract_images<'a>(blob: &Value, images: &'a mut Vec<String>) -> &'a Vec<String> {
    match blob {
        Value::Array(vals) => {
            for v in vals {
                extract_images(v, images);
            }
        }
        Value::Object(m) => {
            for (k, v) in m {
                if k == "image" {
                    if let Value::String(image) = v {
                        images.push(image.to_owned())
                    }
                } else {
                    extract_images(v, images);
                }
            }
        }
        _ => (),
    }
    images
}

// TODO: Each function should have it's own enum of the errors it can return
// There must be a standard pattern for this somewhere...
#[derive(Debug, Fail)]
pub enum RegistryError {
    #[fail(display = "Invalid repository or tag")]
    InvalidName,
    #[fail(display = "Invalid manifest")]
    InvalidManifest,
    #[fail(display = "Invalid Range")]
    Internal,
}

impl ManifestStorage for ClientInterface {
    fn get_manifest(&self, name: &str, tag: &str) -> Result<ManifestReader, StorageDriverError> {
        let mut rt = Runtime::new().unwrap();
        let rn = RepoName(name.to_string());
        let f = self.get_reader_for_manifest(&rn, tag);
        let mr = rt.block_on(f).map_err(|e| {
            warn!("Error getting manifest {:?}", e);
            StorageDriverError::Internal
        })?;

        Ok(mr)
    }

    fn store_manifest(
        &self,
        name: &str,
        tag: &str,
        data: &mut Box<dyn Read>,
    ) -> Result<if_digest, StorageDriverError> {
        let repo = RepoName(name.to_string());

        let mut rt = Runtime::new().unwrap();
        match rt.block_on(self.upload_manifest(&repo, &tag, data)) {
            Ok(vm) => {
                let mut iter = vm.digest().0.split(":");
                let algo = DigestAlgorithm::from_str(iter.next().unwrap_or("sha256"))
                    .unwrap_or(DigestAlgorithm::Sha256);
                let hash = iter
                    .next()
                    .ok_or_else(|| {
                        warn!("Error decoding digest: {}", vm.digest());
                        StorageDriverError::Internal
                    })?
                    .to_string();
                Ok(if_digest { algo, hash })
            }
            Err(RegistryError::InvalidName) => {
                Err(StorageDriverError::InvalidName(format!("{}:{}", name, tag)))
            }
            Err(RegistryError::InvalidManifest) => Err(StorageDriverError::InvalidManifest),
            Err(_) => Err(StorageDriverError::Internal),
        }
    }

    fn delete_manifest(&self, name: &str, digest: &if_digest) -> Result<(), StorageDriverError> {
        let repo = RepoName(name.to_string());
        let digest = Digest(format!("{}", digest));
        let r = self.delete_by_manifest(&repo, &digest);
        Runtime::new().unwrap().block_on(r).map_err(|e| {
            let e = e.downcast::<tonic::Status>();
            if let Ok(ts) = e {
                match ts.code() {
                    Code::InvalidArgument => StorageDriverError::Unsupported,
                    Code::NotFound => StorageDriverError::InvalidManifest,
                    _ => StorageDriverError::Internal,
                }
            } else {
                StorageDriverError::Internal
            }
        })?;
        Ok(())
    }

    fn has_manifest(&self, _name: &str, _algo: &DigestAlgorithm, _reference: &str) -> bool {
        todo!()
    }
}

impl BlobStorage for ClientInterface {
    fn get_blob(&self, name: &str, digest: &if_digest) -> Result<BlobReader, StorageDriverError> {
        let mut rt = Runtime::new().unwrap();
        let rn = RepoName(name.to_string());
        let digest = Digest(format!("{}", digest).to_string());
        let f = self.get_reader_for_blob(&rn, &digest);
        let br = rt.block_on(f).map_err(|e| {
            warn!("Error getting manifest {:?}", e);
            StorageDriverError::Internal
        })?;

        Ok(br)
    }

    fn store_blob_chunk(
        &self,
        name: &str,
        session_id: &str,
        data_info: Option<ContentInfo>,
        data: &mut Box<dyn Read>,
    ) -> Result<u64, StorageDriverError> {
        let mut rt = Runtime::new().unwrap();
        let rn = RepoName(name.to_string());
        let uuid = Uuid(session_id.to_string());
        let f = self.get_write_sink_for_upload(&rn, &uuid);
        let mut sink = rt.block_on(f).map_err(|e| {
            warn!("Error finding write sink for blob {:?}", e);
            StorageDriverError::InvalidName(format!("{} {}", name, session_id))
        })?;

        let have_range = data_info.is_some();
        let info = data_info.unwrap_or(ContentInfo {
            length: 0,
            range: (0, 0),
        });

        let start_index = sink.stream_len().unwrap_or(0);
        if have_range && (start_index != info.range.0) {
            warn!(
                "Asked to store blob with invalid start index. Expected {} got {}",
                start_index, info.range.0
            );
            return Err(StorageDriverError::InvalidContentRange);
        }

        let len = io::copy(data, &mut sink).map_err(|e| {
            warn!("Error writing blob {:?}", e);
            StorageDriverError::Internal
        })?;

        let total = sink.stream_len().unwrap_or(len);
        if have_range {
            if (info.range.1 + 1) != total {
                warn!("total {} r + 1 {}", total, info.range.1 + 1 + 1);
                return Err(StorageDriverError::InvalidContentRange);
            }
            //Check length if chunked upload
            if info.length != len {
                warn!("info.length {} len {}", info.length, len);
                return Err(StorageDriverError::InvalidContentRange);
            }
        }
        Ok(total)
    }

    fn complete_and_verify_blob_upload(
        &self,
        name: &str,
        session_id: &str,
        digest: &if_digest,
    ) -> Result<(), StorageDriverError> {
        let mut rt = Runtime::new().unwrap();

        rt.block_on(self.complete_upload(name, session_id, &digest))
            .map_err(|e| match e.downcast::<tonic::Status>() {
                Ok(ts) => match ts.code() {
                    Code::InvalidArgument => StorageDriverError::InvalidDigest,
                    _ => StorageDriverError::Internal,
                },
                Err(e) => {
                    warn!("Error finalising upload {:?}", e);
                    StorageDriverError::Internal
                }
            })?;
        Ok(())
    }

    fn start_blob_upload(&self, name: &str) -> Result<String, StorageDriverError> {
        let mut rt = Runtime::new().unwrap();
        rt.block_on(self.request_upload(name)).map_err(|e| {
            match e.downcast::<tonic::Status>().map(|s| s.code()) {
                Ok(Code::InvalidArgument) => StorageDriverError::InvalidName(name.to_string()),
                _ => StorageDriverError::Internal,
            }
        })
    }

    fn delete_blob(&self, name: &str, digest: &if_digest) -> Result<(), StorageDriverError> {
        info!("Attempting to delete blob {} in {}", digest, name);
        let rn = RepoName(name.to_string());
        let dig = Digest(digest.to_string());
        let mut rt = Runtime::new().unwrap();
        rt.block_on(self.delete_blob_local(&rn, &dig))
            .map_err(|_| StorageDriverError::InvalidDigest)?;
        Ok(())
    }

    fn status_blob_upload(
        &self,
        _name: &str,
        _session_id: &str,
    ) -> crate::registry_interface::UploadInfo {
        todo!()
    }

    fn cancel_blob_upload(&self, _name: &str, _session_id: &str) -> Result<(), StorageDriverError> {
        todo!()
    }

    fn has_blob(&self, _name: &str, _digest: &if_digest) -> bool {
        todo!()
    }
}

impl CatalogOperations for ClientInterface {
    fn get_catalog(
        &self,
        start_value: Option<&str>,
        num_results: Option<u32>,
    ) -> Result<Vec<String>, StorageDriverError> {
        let num_results = num_results.unwrap_or(u32::MAX);
        let start_value = start_value.unwrap_or_default();

        Runtime::new()
            .unwrap()
            .block_on(self.get_catalog_part(num_results, start_value))
            .map_err(|_| StorageDriverError::Internal)
            .map(|rc| rc.raw())
    }

    fn get_tags(
        &self,
        repo: &str,
        start_value: Option<&str>,
        num_results: Option<u32>,
    ) -> Result<Vec<String>, StorageDriverError> {
        let num_results = num_results.unwrap_or(u32::MAX);
        let start_value = start_value.unwrap_or_default();

        Runtime::new()
            .unwrap()
            .block_on(self.list_tags(repo, num_results, start_value))
            .map_err(|_| StorageDriverError::Internal)
            .map(|rc| rc.raw())
    }

    fn get_history(
        &self,
        repo: &str,
        name: &str,
        start_value: Option<&str>,
        num_results: Option<u32>,
    ) -> Result<ManifestHistory, StorageDriverError> {
        let num_results = num_results.unwrap_or(u32::MAX);
        let start_value = start_value.unwrap_or_default();

        Runtime::new()
            .unwrap()
            .block_on(self.get_manifest_history(repo, name, num_results, start_value))
            .map_err(|_| StorageDriverError::Internal)
    }
}

impl Validation for ClientInterface {
    fn validate_admission(
        &self,
        admission_req: &AdmissionRequest,
        host_names: &Vec<String>,
    ) -> Result<AdmissionResponse, ValidationError> {
        Runtime::new()
            .unwrap()
            .block_on(self.validate_admission_internal(admission_req, host_names))
            .map_err(|_| ValidationError::Internal)
    }
}

impl Metrics for ClientInterface {
    fn is_healthy(&self) -> bool {
        Runtime::new()
            .unwrap()
            .block_on(self.is_healthy())
            .is_healthy
    }

    fn is_ready(&self) -> bool {
        Runtime::new().unwrap().block_on(self.is_ready()).is_ready
    }

    fn get_metrics(&self) -> Result<MetricsResponse, crate::registry_interface::MetricsError> {
        Runtime::new()
            .unwrap()
            .block_on(self.get_metrics())
            .map_err(|_| MetricsError::Internal)
    }
}

impl ClientInterface {
    pub fn new(server: String) -> Result<Self, Error> {
        Ok(ClientInterface { server })
    }

    async fn connect_registry(
        &self,
    ) -> Result<RegistryClient<tonic::transport::Channel>, tonic::transport::Error> {
        debug!("Connecting to {}", self.server);
        let x = RegistryClient::connect(self.server.to_string()).await;
        debug!("Connected to {}", self.server);
        x
    }

    async fn connect_admission_controller(
        &self,
    ) -> Result<AdmissionControllerClient<tonic::transport::Channel>, tonic::transport::Error> {
        debug!("Connecting to {}", self.server);
        let x = AdmissionControllerClient::connect(self.server.to_string()).await;
        debug!("Connected to {}", self.server);
        x
    }

    async fn request_upload(&self, repo_name: &str) -> Result<String, Error> {
        info!("Request Upload called for {}", repo_name);
        let req = UploadRequest {
            repo_name: repo_name.to_string(),
        };

        let response = self
            .connect_registry()
            .await?
            .request_upload(Request::new(req))
            .await?
            .into_inner();

        Ok(response.uuid)
    }

    async fn complete_upload(
        &self,
        repo_name: &str,
        uuid: &str,
        digest: &if_digest,
    ) -> Result<(), Error> {
        info!(
            "Complete Upload called for repository {} with upload id {} digest {}",
            repo_name, uuid, digest
        );

        let req = CompleteRequest {
            repo_name: repo_name.to_string(),
            uuid: uuid.to_string(),
            user_digest: digest.to_string(),
        };

        self.connect_registry()
            .await?
            .complete_upload(Request::new(req))
            .await?;

        Ok(())
    }

    async fn get_write_sink_for_upload(
        &self,
        repo_name: &RepoName,
        uuid: &Uuid,
    ) -> Result<impl Write + Seek, Error> {
        info!(
            "Getting write location for blob in repo {} with upload id {}",
            repo_name, uuid
        );
        let br = UploadRef {
            uuid: uuid.0.clone(),
            repo_name: repo_name.0.clone(),
        };

        let resp = self
            .connect_registry()
            .await?
            .get_write_location_for_blob(Request::new(br))
            .await?
            .into_inner();

        //For the moment we know it's a file location
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(resp.path)?;
        Ok(file)
    }

    async fn upload_manifest<'a>(
        &self,
        repo_name: &RepoName,
        reference: &str,
        manifest: &mut Box<dyn Read + 'a>,
    ) -> Result<types::VerifiedManifest, RegistryError> {
        let (mut sink_loc, uuid) = self
            .get_write_sink_for_manifest(repo_name, reference)
            .await
            .map_err(|e| {
                let e = e.downcast::<tonic::Status>();
                if let Ok(ts) = e {
                    match ts.code() {
                        Code::InvalidArgument => RegistryError::InvalidName,
                        _ => RegistryError::Internal,
                    }
                } else {
                    RegistryError::Internal
                }
            })?;

        io::copy(manifest, &mut sink_loc).map_err(|e| {
            warn!("Error wirting out manifest {:?}", e);
            RegistryError::Internal
        })?;

        self.verify_manifest(repo_name, reference, &uuid)
            .await
            .map_err(|e| {
                let e = e.downcast::<tonic::Status>();
                if let Ok(ts) = e {
                    match ts.code() {
                        Code::InvalidArgument => RegistryError::InvalidManifest,
                        _ => RegistryError::Internal,
                    }
                } else {
                    RegistryError::Internal
                }
            })
    }

    async fn get_write_sink_for_manifest(
        &self,
        repo_name: &RepoName,
        reference: &str,
    ) -> Result<(impl Write, String), Error> {
        info!(
            "Getting write location for manifest in repo {} with ref {}",
            repo_name, reference
        );
        let mr = ManifestRef {
            reference: reference.to_owned(),
            repo_name: repo_name.0.clone(),
        };

        let resp = self
            .connect_registry()
            .await?
            .get_write_details_for_manifest(Request::new(mr))
            .await?
            .into_inner();

        //For the moment we know it's a file location
        //Manifests don't append; just overwrite
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(resp.path)?;
        Ok((file, resp.uuid))
    }

    async fn get_reader_for_manifest(
        &self,
        repo_name: &RepoName,
        reference: &str,
    ) -> Result<ManifestReader, Error> {
        info!(
            "Getting read location for {} with ref {}",
            repo_name, reference
        );
        let mr = ManifestRef {
            reference: reference.to_owned(),
            repo_name: repo_name.0.clone(),
        };
        let resp = self
            .connect_registry()
            .await?
            .get_read_location_for_manifest(Request::new(mr))
            .await?
            .into_inner();

        //For the moment we know it's a file location
        let file = OpenOptions::new().read(true).open(resp.path)?;
        let mr = create_manifest_reader(Box::new(file), resp.content_type, Digest(resp.digest));
        Ok(mr)
    }

    async fn get_manifest_history(
        &self,
        repo_name: &str,
        reference: &str,
        limit: u32,
        last_digest: &str,
    ) -> Result<ManifestHistory, Error> {
        info!(
            "Getting manifest history for repo {} ref {} limit {} last_digest {}",
            repo_name, reference, limit, last_digest
        );
        let mr = ManifestHistoryRequest {
            tag: reference.to_owned(),
            repo_name: repo_name.to_string(),
            limit,
            last_digest: last_digest.to_owned(),
        };
        let mut stream = self
            .connect_registry()
            .await?
            .get_manifest_history(Request::new(mr))
            .await?
            .into_inner();
        let mut history = ManifestHistory::new(format!("{}:{}", repo_name, reference));

        while let Some(entry) = stream.message().await? {
            let ts = if let Some(date) = entry.date {
                chrono::Utc.timestamp(date.seconds, date.nanos.try_into().unwrap())
            } else {
                warn!("Manifest digest stored without timestamp. Using Epoch.");
                chrono::Utc.timestamp(0, 0)
            };
            history.insert(entry.digest, ts);
        }

        Ok(history)
    }

    async fn get_reader_for_blob(
        &self,
        repo_name: &RepoName,
        digest: &Digest,
    ) -> Result<BlobReader, Error> {
        info!("Getting read location for blob {} in {}", digest, repo_name);
        let br = BlobRef {
            digest: digest.0.clone(),
            repo_name: repo_name.0.clone(),
        };

        let resp = self
            .connect_registry()
            .await?
            .get_read_location_for_blob(Request::new(br))
            .await?
            .into_inner();

        //For the moment we know it's a file location
        let file = OpenOptions::new().read(true).open(resp.path)?;
        let reader = create_blob_reader(Box::new(file), digest.clone());
        Ok(reader)
    }

    async fn delete_blob_local(
        &self,
        repo_name: &RepoName,
        digest: &Digest,
    ) -> Result<BlobDeleted, Error> {
        info!("Attempting to delete blob {} in {}", digest, repo_name);
        let br = BlobRef {
            digest: digest.0.clone(),
            repo_name: repo_name.0.clone(),
        };

        self.connect_registry()
            .await?
            .delete_blob(Request::new(br))
            .await?
            .into_inner();
        Ok(BlobDeleted {})
    }

    async fn verify_manifest(
        &self,
        repo_name: &RepoName,
        reference: &str,
        uuid: &str,
    ) -> Result<types::VerifiedManifest, Error> {
        info!(
            "Verifying manifest {} in {} uuid {}",
            reference, repo_name, uuid
        );
        let vmr = VerifyManifestRequest {
            manifest: Some(ManifestRef {
                reference: reference.to_owned(),
                repo_name: repo_name.0.clone(),
            }),
            uuid: uuid.to_string(),
        };

        let resp = self
            .connect_registry()
            .await?
            .verify_manifest(Request::new(vmr))
            .await?
            .into_inner();

        let vm = create_verified_manifest(
            repo_name.clone(),
            Digest(resp.digest.to_owned()),
            reference.to_string(),
        );
        Ok(vm)
    }

    async fn delete_by_manifest(
        &self,
        repo_name: &RepoName,
        digest: &Digest,
    ) -> Result<ManifestDeleted, Error> {
        info!("Attempting to delete manifest {} in {}", digest, repo_name);
        let mr = ManifestRef {
            reference: digest.0.clone(),
            repo_name: repo_name.0.clone(),
        };

        self.connect_registry()
            .await?
            .delete_manifest(Request::new(mr))
            .await?
            .into_inner();
        Ok(ManifestDeleted {})
    }

    async fn get_catalog_part(&self, limit: u32, last_repo: &str) -> Result<RepoCatalog, Error> {
        info!(
            "Getting image catalog limit {} last_repo {}",
            limit, last_repo
        );

        let cr = CatalogRequest {
            limit,
            last_repo: last_repo.to_string(),
        };
        let mut stream = self
            .connect_registry()
            .await?
            .get_catalog(Request::new(cr))
            .await?
            .into_inner();
        let mut catalog = RepoCatalog::new();

        while let Some(ce) = stream.message().await? {
            catalog.insert(ce.repo_name.to_owned());
        }

        Ok(catalog)
    }

    async fn list_tags(
        &self,
        repo_name: &str,
        limit: u32,
        last_tag: &str,
    ) -> Result<TagList, Error> {
        info!(
            "Getting tag list for {} limit {} last_tag {}",
            repo_name, limit, last_tag
        );
        let ltr = ListTagsRequest {
            repo_name: repo_name.to_string(),
            limit,
            last_tag: last_tag.to_string(),
        };

        let mut stream = self
            .connect_registry()
            .await?
            .list_tags(Request::new(ltr))
            .await?
            .into_inner();
        let mut list = TagList::new(repo_name.to_string());

        while let Some(tag) = stream.message().await? {
            list.insert(tag.tag.to_owned());
        }

        Ok(list)
    }

    /**
     * Returns an AdmissionReview object with the AdmissionResponse completed with details of vaildation.
     */
    async fn validate_admission_internal(
        &self,
        req: &types::AdmissionRequest,
        host_names: &[String],
    ) -> Result<types::AdmissionResponse, Error> {
        info!(
            "Validating admission request {} host_names {:?}",
            req.uid, host_names
        );
        //TODO: write something to convert automatically (into()) between AdmissionRequest types
        // TODO: we should really be sending the full object to the backend.
        let mut images = Vec::new();
        extract_images(&req.object, &mut images);
        let ar = trow_proto::AdmissionRequest {
            images,
            namespace: req.namespace.clone(),
            operation: req.operation.clone(),
            host_names: host_names.to_vec(),
        };

        let resp = self
            .connect_admission_controller()
            .await?
            .validate_admission(Request::new(ar))
            .await?
            .into_inner();

        //TODO: again, this should be an automatic conversion
        let st = if resp.is_allowed {
            types::Status {
                status: "Success".to_owned(),
                message: None,
                code: None,
            }
        } else {
            //Not sure "Failure" is correct
            types::Status {
                status: "Failure".to_owned(),
                message: Some(resp.reason.to_string()),
                code: None,
            }
        };
        Ok(types::AdmissionResponse {
            uid: req.uid.clone(),
            allowed: resp.is_allowed,
            status: Some(st),
        })
    }

    /**
    Health check.

    Note that the server will indicate unhealthy by returning an error.
    */
    async fn is_healthy(&self) -> types::HealthResponse {
        debug!("Calling health check");
        let mut client = match self.connect_registry().await {
            Ok(cl) => cl,
            Err(_) => {
                return types::HealthResponse {
                    is_healthy: false,
                    message: "Failed to connect to registry".to_string(),
                }
            }
        };

        let req = Request::new(HealthRequest {});
        let resp = match client.is_healthy(req).await {
            Ok(r) => r,
            Err(e) => {
                return types::HealthResponse {
                    is_healthy: false,
                    message: e.to_string(),
                }
            }
        };
        let response_value = resp.into_inner();

        types::HealthResponse {
            is_healthy: true,
            message: response_value.message,
        }
    }

    /**
     Readiness check.

     Note that the server will indicate not ready by returning an error.
    */
    async fn is_ready(&self) -> types::ReadinessResponse {
        debug!("Calling readiness check");
        let mut client = match self.connect_registry().await {
            Ok(cl) => cl,
            Err(_) => {
                return types::ReadinessResponse {
                    is_ready: false,
                    message: "Failed to connect to registry".to_string(),
                }
            }
        };

        let req = Request::new(ReadinessRequest {});
        let resp = match client.is_ready(req).await {
            Ok(r) => r,
            Err(e) => {
                return types::ReadinessResponse {
                    is_ready: false,
                    message: e.to_string(),
                }
            }
        };
        let response_value = resp.into_inner();
        types::ReadinessResponse {
            is_ready: true,
            message: response_value.message,
        }
    }

    /**
     Metrics call.

     Returns disk and total request metrics(blobs, manifests).
    */
    async fn get_metrics(&self) -> Result<types::MetricsResponse, Error> {
        debug!("Getting metrics");
        let req = Request::new(MetricsRequest {});
        let resp = self
            .connect_registry()
            .await?
            .get_metrics(req)
            .await?
            .into_inner();

        Ok(types::MetricsResponse {
            metrics: resp.metrics,
        })
    }
}
