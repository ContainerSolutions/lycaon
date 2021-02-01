pub mod trow_proto {
    include!("../lib/protobuf/out/trow.rs");
}

use trow_proto::{
    admission_controller_client::AdmissionControllerClient, registry_client::RegistryClient,
    BlobRef, CatalogRequest, CompleteRequest, HealthRequest, ListTagsRequest,
    ManifestHistoryRequest, ManifestRef, MetricsRequest, ReadinessRequest, UploadRef,
    UploadRequest, VerifyManifestRequest,
};

use tonic::{Code, Request};

use crate::chrono::TimeZone;
use crate::types::{self, *};
use failure::Error;
use serde_json::Value;
use std::convert::TryInto;
use std::fs::OpenOptions;
use std::io;
use std::io::prelude::*;

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
    #[fail(display = "Invalid name or UUID")]
    InvalidNameOrUUID,
    #[fail(display = "Invalid manifest")]
    InvalidManifest,
    #[fail(display = "Internal Error")]
    InvalidContentRange,
    #[fail(display = "Internal Error")]
    Internal,
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

    /**
     * Ok so these functions are largely boilerplate to call the GRPC backend.
     * But doing it here lets us change things behind the scenes much cleaner.
     *
     * Frontend code becomes smaller and doesn't need to know about GRPC types.
     * In fact you could pull it out for a different implementation now by
     * just changing this file...
     **/

    pub async fn request_upload(&self, repo_name: &RepoName) -> Result<UploadInfo, Error> {
        info!("Request Upload called for {}", repo_name);
        let req = UploadRequest {
            repo_name: repo_name.0.clone(),
        };

        let response = self
            .connect_registry()
            .await?
            .request_upload(Request::new(req))
            .await?
            .into_inner();

        Ok(create_upload_info(
            types::Uuid(response.uuid),
            repo_name.clone(),
            (0, 0),
        ))
    }

    pub async fn upload_blob<'a>(
        &self,
        repo_name: &RepoName,
        uuid: &Uuid,
        digest: &str,
        blob: &mut Box<dyn Read + 'a>,
    ) -> Result<AcceptedUpload, RegistryError> {
        let mut sink = self
            .get_write_sink_for_upload(repo_name, &uuid)
            .await
            .map_err(|e| {
                warn!("Error finding write sink for blob {:?}", e);
                RegistryError::InvalidNameOrUUID
            })?;
        let len = io::copy(blob, &mut sink).map_err(|e| {
            warn!("Error writing blob {:?}", e);
            RegistryError::Internal
        })?;
        let digest = Digest(digest.to_string());
        self.complete_upload(repo_name, uuid, &digest, len)
            .await
            .map_err(|e| {
                warn!("Error finalising upload {:?}", e);
                RegistryError::Internal
            })
    }

    pub async fn upload_blob_chunk<'a>(
        &self,
        repo_name: &RepoName,
        uuid: &Uuid,
        info: Option<ContentInfo>,
        chunk: &mut Box<dyn Read + 'a>,
    ) -> Result<UploadInfo, RegistryError> {
        let mut sink = self
            .get_write_sink_for_upload(repo_name, &uuid)
            .await
            .map_err(|e| {
                warn!("Error finding write sink for blob {:?}", e);
                RegistryError::InvalidNameOrUUID
            })?;

        let have_chunked_upload = info.is_some();
        let info = info.unwrap_or(ContentInfo {
            length: 0,
            range: (0, 0),
        });

        let start_index = sink.stream_len().unwrap_or(0);
        if start_index != info.range.0 {
            warn!(
                "Asked for blob upload with invalid start index. Expected {} got {}",
                start_index, info.range.0
            );
            return Err(RegistryError::InvalidContentRange);
        }

        let len = io::copy(chunk, &mut sink).map_err(|e| {
            warn!("Error writing blob {:?}", e);
            RegistryError::Internal
        })?;
        let total = sink.stream_len().unwrap_or(len);
        if have_chunked_upload {
            if (info.range.1 + 1) != total {
                warn!("total {} r + 1 {}", total, info.range.1 + 1 + 1);
                return Err(RegistryError::InvalidContentRange);
            }
            //Check length if chunked upload
            if info.length != len {
                warn!("info.length {} len {}", info.length, len);
                return Err(RegistryError::InvalidContentRange);
            }
        }
        Ok(create_upload_info(
            uuid.clone(),
            repo_name.clone(),
            (0, total as u32),
        ))
    }

    async fn complete_upload(
        &self,
        repo_name: &RepoName,
        uuid: &Uuid,
        digest: &Digest,
        len: u64,
    ) -> Result<AcceptedUpload, Error> {
        info!(
            "Complete Upload called for repository {} with upload id {} digest {} and length {}",
            repo_name, uuid, digest, len
        );
        let req = CompleteRequest {
            repo_name: repo_name.0.clone(),
            uuid: uuid.0.clone(),
            user_digest: digest.0.clone(),
        };
        let resp = self
            .connect_registry()
            .await?
            .complete_upload(Request::new(req))
            .await?
            .into_inner();

        Ok(create_accepted_upload(
            Digest(resp.digest),
            repo_name.clone(),
            uuid.clone(),
            (0, (len as u32)),
        ))
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

    pub async fn upload_manifest<'a>(
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

    pub async fn get_reader_for_manifest(
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

    pub async fn get_manifest_history(
        &self,
        repo_name: &RepoName,
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
            repo_name: repo_name.0.clone(),
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

    pub async fn get_reader_for_blob(
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

    pub async fn delete_blob(
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

    pub async fn verify_manifest(
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
            resp.content_type,
        );
        Ok(vm)
    }

    pub async fn delete_manifest(
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

    pub async fn get_catalog(&self, limit: u32, last_repo: &str) -> Result<RepoCatalog, Error> {
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
            catalog.insert(RepoName(ce.repo_name.to_owned()));
        }

        Ok(catalog)
    }

    pub async fn list_tags(
        &self,
        repo_name: &RepoName,
        limit: u32,
        last_tag: &str,
    ) -> Result<TagList, Error> {
        info!(
            "Getting tag list for {} limit {} last_tag {}",
            repo_name, limit, last_tag
        );
        let ltr = ListTagsRequest {
            repo_name: repo_name.0.clone(),
            limit,
            last_tag: last_tag.to_string(),
        };

        let mut stream = self
            .connect_registry()
            .await?
            .list_tags(Request::new(ltr))
            .await?
            .into_inner();
        let mut list = TagList::new(repo_name.clone());

        while let Some(tag) = stream.message().await? {
            list.insert(tag.tag.to_owned());
        }

        Ok(list)
    }

    /**
     * Returns an AdmissionReview object with the AdmissionResponse completed with details of vaildation.
     */
    pub async fn validate_admission(
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
    pub async fn is_healthy(&self) -> types::HealthResponse {
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
    pub async fn is_ready(&self) -> types::ReadinessResponse {
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
    pub async fn get_metrics(&self) -> Result<types::MetricsResponse, Error> {
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
