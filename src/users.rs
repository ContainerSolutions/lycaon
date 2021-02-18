use argon2::{self, Config};
use bytes::Bytes;
use data_encoding::HEXUPPER;
use failure;
use rand;
use rusqlite::NO_PARAMS;
use rusqlite::{params, Connection};
use std::env;
extern crate serde_yaml;

// User Struct
pub struct User {
    pub name: String,
    pub salt: String,
    pub hash: String,
    pub active: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileUser {
    name: String,
    password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileUsers {
    users: Vec<FileUser>,
}

// Constants
const CREDENTIAL_LEN: usize = 512;

// Error Used for User related Functions
// Implements the traits of Error

// Generates a salt
fn get_salt() -> Vec<u8> {
    let salt: Vec<u8> = (0..CREDENTIAL_LEN).map(|_| rand::random::<u8>()).collect();
    salt
}

// Takes in a salt and password and returns the hash
// using the argon 2 algorithm
fn get_hash_from_password(password: String, salt: Vec<u8>) -> Result<String, failure::Error> {
    let config = Config::default();
    let hash = argon2::hash_encoded(password.as_bytes(), &Bytes::from(salt), &config)?;
    Ok(hash)
}

// Verifies a password and salt against a hash
// using the argon 2 algorithm
fn verify_password(password: String, hash: String) -> Result<bool, failure::Error> {
    if hash.is_empty() {
        return Ok(false);
    }
    Ok(argon2::verify_encoded(&hash, password.as_bytes())?)
}

// creates a sqlite DB if it does not exist and initializes the user table
// returns the connection
fn connection() -> Result<rusqlite::Connection, failure::Error> {
    let db_file = env::var("DB_FILE").unwrap_or("sqlite.db".to_string());
    let conn = Connection::open(db_file).expect("db conn fail");

    // create the users table if it does not exist
    conn.execute(
        "create table if not exists users (
             id integer primary key,
             name text not null unique,
             salt text not null,
             hash text not null,
             active integer default 1 not null
         );",
        NO_PARAMS,
    )?;
    Ok(conn)
}

impl User {
    // trait used to create a user
    fn new(username: String, password: String) -> Result<User, failure::Error> {
        // Generates a salt for the user
        let salt = get_salt();
        let user = User {
            name: username,
            salt: HEXUPPER.encode(&salt),
            // Generates a hash from the salt and password
            hash: get_hash_from_password(password, salt)?,
            active: 1,
        };
        // Connect to the db
        let conn = connection()?;
        // Insert User into the db
        conn.execute(
            "INSERT INTO users (name, salt, hash, active) VALUES (?1, ?2, ?3, ?4)",
            params![user.name, user.salt, user.hash, user.active],
        )?;
        Ok(user)
    }
    // Trait to Authorize the User
    fn authorize(username: String, password: String) -> Result<User, failure::Error> {
        // Connect to DB
        let conn = connection()?;
        // Prepare Select statement
        let mut stmt = conn
            .prepare("SELECT id, name, salt, hash, active FROM users WHERE name = ?1 LIMIT 1;")?;
        // Run the Query Passing in the user name should only return a single row
        let mut user = User {
            name: "".to_string(),
            salt: "".to_string(),
            hash: "".to_string(),
            active: 0,
        };
        let user_iter = stmt.query_map(params![username], |row| {
            Ok(User {
                name: row.get(1)?,
                salt: row.get(2)?,
                hash: row.get(3)?,
                active: row.get(4)?,
            })
        })?;
        for record in user_iter {
            let db_user = record?;

            user.hash = db_user.hash.clone();
            user.salt = db_user.salt.clone();
            user.active = db_user.active.clone();
        }
        // Verify Password against hash using salt
        let valid = verify_password(password, user.hash.clone())?;
        match valid {
            true => return Ok(user),
            false => return Err(format_err!("Invalid Credentials")),
        }
    }

    // TODO: Implement update feature
    // pub fn update(&mut self) {}

    fn delete(username: String) -> Result<(), failure::Error> {
        let conn = connection()?;
        conn.execute("DELETE FROM users WHERE name = ?1;", params![username])?;
        // unset current user
        Ok(())
    }

    fn load_users_from_file() -> Result<(), failure::Error> {
        let users_file = env::var("USER_CONFIG").unwrap_or("users.yaml".to_string());
        let f = std::fs::read_to_string(users_file)?;
        let file_contents = serde_yaml::from_str::<FileUsers>(&f)?;
        for user in file_contents.users {
            match User::new(user.name, user.password) {
                Ok(_) => {}
                Err(e) => return Err(format_err!("Failed Loading All users {}", e)),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use super::User;
    use super::{get_hash_from_password, get_salt, verify_password, FileUser, FileUsers};
    use std::env;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn test_get_salt() {
        let salt = get_salt();
        assert!(
            salt.len() == 512,
            "Expected Salt to be of length {} but got {}",
            512,
            salt.len()
        )
    }

    #[test]
    fn test_hashing_password() {
        let salt = get_salt();
        match get_hash_from_password(String::from("Password1"), salt.clone()) {
            Ok(hash) => {
                // testing valid password
                match verify_password(String::from("Password1"), hash.clone()) {
                    Ok(valid) => assert!(valid, "Expected password to get verified"),
                    _ => {}
                }
                // testing invalid password
                match verify_password(String::from("Password2"), hash) {
                    Ok(invalid) => assert!(!invalid, "Expected password to fail verification"),
                    _ => {}
                }
            }
            Err(_) => {
                assert!(false, "Failed to get hash from password");
            }
        }
    }

    #[test]
    fn test_create_and_get() {
        env::set_var("DB_FILE", "test.db");
        assert!(
            User::new("spazzy".to_string(), "Password1".to_string()).is_ok(),
            "Failed Creating User"
        );
        assert!(
            User::authorize("spazzy".to_string(), "Password1".to_string()).is_ok(),
            "Failed Authenticating User"
        );
        assert!(
            User::delete("spazzy".to_string()).is_ok(),
            "Failed Cleaning Up User"
        );
    }

    #[test]
    fn test_cannot_create_duplicate_user() {
        env::set_var("DB_FILE", "test.db");
        assert!(
            User::new("user_one".to_string(), "Password1".to_string()).is_ok(),
            "Failed To create User"
        );
        assert!(
            !User::new("user_one".to_string(), "Password1".to_string()).is_ok(),
            "Should have received an error for duplicate user"
        );
        assert!(
            User::delete("user_one".to_string()).is_ok(),
            "Failed Cleaning Up User"
        );
    }

    #[test]
    fn test_auth_non_existant_user() {
        env::set_var("DB_FILE", "test.db");
        assert!(
            !User::authorize("non_existant".to_string(), "Password1".to_string()).is_ok(),
            "Non Existant User passed auth"
        );
    }

    #[test]
    fn test_can_delete_user() {
        env::set_var("DB_FILE", "test.db");
        assert!(
            User::new("user_delete".to_string(), "Password1".to_string()).is_ok(),
            "Failed top create User"
        );
        assert!(
            User::delete("user_delete".to_string()).is_ok(),
            "Failed during removal of user"
        );
        assert!(
            !User::authorize("user_delete".to_string(), "Password1".to_string()).is_ok(),
            "User was not deleted"
        );
    }

    #[test]
    fn test_load_users_from_yaml() {
        // Setup
        env::set_var("DB_FILE", "test.db");
        env::set_var("USER_CONFIG", "test_users.yaml");
        // Create the yaml File
        let file_user = FileUser {
            name: "file_user".to_string(),
            password: "123456789".to_string(),
        };
        let mut file_users = Vec::new();
        file_users.push(file_user);
        let file = FileUsers { users: file_users };
        let yaml_string = serde_yaml::to_string(&file).expect("Failed Creating Yaml String");
        match File::create("test_users.yaml") {
            Ok(mut file) => file
                .write_all(yaml_string.as_bytes())
                .expect("Unable to write data"),
            Err(_) => assert!(false, "Failed Creating file"),
        }
        // Test Loading the yaml file into db
        assert!(
            User::load_users_from_file().is_ok(),
            "Failed loading users from file"
        );
        // clean up yaml file when no longer needed
        std::fs::remove_file("test_users.yaml").expect("Failed Cleaning Up File");
        // Authorize the user that was loaded
        assert!(
            User::authorize("file_user".to_string(), "123456789".to_string()).is_ok(),
            "Failed authenticating Loaded User"
        );
        // Clean Up the user from the DB
        assert!(
            User::delete("filter_user".to_string()).is_ok(),
            "Failed Cleaning Up User"
        );
    }
}
