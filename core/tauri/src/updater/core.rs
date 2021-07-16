// Copyright 2019-2021 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use super::error::{Error, Result};
use crate::api::{file::Extract, version};
use base64::decode;
use http::StatusCode;
use minisign_verify::{PublicKey, Signature};
use std::{
  collections::HashMap,
  env,
  ffi::OsStr,
  fs::{read_dir, remove_file, File, OpenOptions},
  io::{prelude::*, BufReader, Read},
  path::{Path, PathBuf},
  str::from_utf8,
  time::{SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "macos")]
use std::fs::rename;
#[cfg(not(target_os = "macos"))]
use std::process::Command;

#[cfg(target_os = "macos")]
use crate::api::file::Move;

use crate::api::http::{ClientBuilder, HttpRequestBuilder};

#[cfg(target_os = "windows")]
use std::process::exit;

#[derive(Debug)]
pub struct RemoteRelease {
  /// Version to install
  pub version: String,
  /// Release date
  pub date: String,
  /// Download URL for current platform
  pub download_url: String,
  /// Update short description
  pub body: Option<String>,
  /// Optional signature for the current platform
  pub signature: Option<String>,
}

impl RemoteRelease {
  // Read JSON and confirm this is a valid Schema
  fn from_release(release: &serde_json::Value, target: &str) -> Result<RemoteRelease> {
    // Version or name is required for static and dynamic JSON
    // if `version` is not announced, we fallback to `name` (can be the tag name example v1.0.0)
    let version = match release.get("version") {
      Some(version) => version
        .as_str()
        .ok_or_else(|| {
          Error::RemoteMetadata("Unable to extract `version` from remote server".into())
        })?
        .trim_start_matches('v')
        .to_string(),
      None => release
        .get("name")
        .ok_or_else(|| Error::RemoteMetadata("Release missing `name` and `version`".into()))?
        .as_str()
        .ok_or_else(|| {
          Error::RemoteMetadata("Unable to extract `name` from remote server`".into())
        })?
        .trim_start_matches('v')
        .to_string(),
    };

    // pub_date is required default is: `N/A` if not provided by the remote JSON
    let date = match release.get("pub_date") {
      Some(pub_date) => pub_date.as_str().unwrap_or("N/A").to_string(),
      None => "N/A".to_string(),
    };

    // body is optional to build our update
    let body = release
      .get("notes")
      .map(|notes| notes.as_str().unwrap_or("").to_string());

    // signature is optional to build our update
    let mut signature = release
      .get("signature")
      .map(|signature| signature.as_str().unwrap_or("").to_string());

    let download_url;

    match release.get("platforms") {
      //
      // Did we have a platforms field?
      // If we did, that mean it's a static JSON.
      // The main difference with STATIC and DYNAMIC is static announce ALL platforms
      // and dynamic announce only the current platform.
      //
      // This could be used if you do NOT want an update server and use
      // a GIST, S3 or any static JSON file to announce your updates.
      //
      // Notes:
      // Dynamic help to reduce bandwidth usage or to intelligently update your clients
      // based on the request you give. The server can remotely drive behaviors like
      // rolling back or phased rollouts.
      //
      Some(platforms) => {
        // make sure we have our target available
        if let Some(current_target_data) = platforms.get(target) {
          // use provided signature if available
          signature = current_target_data
            .get("signature")
            .map(|found_signature| found_signature.as_str().unwrap_or("").to_string());
          // Download URL is required
          download_url = current_target_data
            .get("url")
            .ok_or_else(|| Error::RemoteMetadata("Release missing `url`".into()))?
            .as_str()
            .ok_or_else(|| {
              Error::RemoteMetadata("Unable to extract `url` from remote server`".into())
            })?
            .to_string();
        } else {
          // make sure we have an available platform from the static
          return Err(Error::RemoteMetadata("Platform not available".into()));
        }
      }
      // We don't have the `platforms` field announced, let's assume our
      // download URL is at the root of the JSON.
      None => {
        download_url = release
          .get("url")
          .ok_or_else(|| Error::RemoteMetadata("Release missing `url`".into()))?
          .as_str()
          .ok_or_else(|| {
            Error::RemoteMetadata("Unable to extract `url` from remote server`".into())
          })?
          .to_string();
      }
    }
    // Return our formatted release
    Ok(RemoteRelease {
      version,
      date,
      download_url,
      body,
      signature,
    })
  }
}

pub struct UpdateBuilder<'a> {
  /// Current version we are running to compare with announced version
  pub current_version: &'a str,
  /// The URLs to checks updates. We suggest at least one fallback on a different domain.
  pub urls: Vec<String>,
  /// The platform the updater will check and install the update. Default is from `get_updater_target`
  pub target: Option<String>,
  /// The current executable path. Default is automatically extracted.
  pub executable_path: Option<PathBuf>,
}

impl<'a> Default for UpdateBuilder<'a> {
  fn default() -> Self {
    UpdateBuilder {
      urls: Vec::new(),
      target: None,
      executable_path: None,
      current_version: env!("CARGO_PKG_VERSION"),
    }
  }
}

// Create new updater instance and return an Update
impl<'a> UpdateBuilder<'a> {
  pub fn new() -> Self {
    UpdateBuilder::default()
  }

  #[allow(dead_code)]
  pub fn url(mut self, url: String) -> Self {
    self.urls.push(url);
    self
  }

  /// Add multiple URLS at once inside a Vec for future reference
  pub fn urls(mut self, urls: &[String]) -> Self {
    let mut formatted_vec: Vec<String> = Vec::new();
    for url in urls {
      formatted_vec.push(url.to_owned());
    }
    self.urls = formatted_vec;
    self
  }

  /// Set the current app version, used to compare against the latest available version.
  /// The `cargo_crate_version!` macro can be used to pull the version from your `Cargo.toml`
  pub fn current_version(mut self, ver: &'a str) -> Self {
    self.current_version = ver;
    self
  }

  /// Set the target (os)
  /// win32, win64, darwin and linux are currently supported
  #[allow(dead_code)]
  pub fn target(mut self, target: &str) -> Self {
    self.target = Some(target.to_owned());
    self
  }

  /// Set the executable path
  #[allow(dead_code)]
  pub fn executable_path<A: AsRef<Path>>(mut self, executable_path: A) -> Self {
    self.executable_path = Some(PathBuf::from(executable_path.as_ref()));
    self
  }

  pub async fn build(self) -> Result<Update> {
    let mut remote_release: Option<RemoteRelease> = None;

    // make sure we have at least one url
    if self.urls.is_empty() {
      return Err(Error::Builder(
        "Unable to check update, `url` is required.".into(),
      ));
    };

    // set current version if not set
    let current_version = self.current_version;

    // If no executable path provided, we use current_exe from rust
    let executable_path = if let Some(v) = &self.executable_path {
      v.clone()
    } else {
      // we expect it to fail if we can't find the executable path
      // without this path we can't continue the update process.
      env::current_exe()?
    };

    // Did the target is provided by the config?
    // Should be: linux, darwin, win32 or win64
    let target = if let Some(t) = &self.target {
      t.clone()
    } else {
      get_updater_target().ok_or(Error::UnsupportedPlatform)?
    };

    // Get the extract_path from the provided executable_path
    let extract_path = extract_path_from_executable(&executable_path);

    // Set SSL certs for linux if they aren't available.
    // We do not require to recheck in the download_and_install as we use
    // ENV variables, we can expect them to be set for the second call.
    #[cfg(target_os = "linux")]
    {
      if env::var_os("SSL_CERT_FILE").is_none() {
        env::set_var("SSL_CERT_FILE", "/etc/ssl/certs/ca-certificates.crt");
      }
      if env::var_os("SSL_CERT_DIR").is_none() {
        env::set_var("SSL_CERT_DIR", "/etc/ssl/certs");
      }
    }

    // Allow fallback if more than 1 urls is provided
    let mut last_error: Option<Error> = None;
    for url in &self.urls {
      // replace {{current_version}} and {{target}} in the provided URL
      // this is usefull if we need to query example
      // https://releases.myapp.com/update/{{target}}/{{current_version}}
      // will be transleted into ->
      // https://releases.myapp.com/update/darwin/1.0.0
      // The main objective is if the update URL is defined via the Cargo.toml
      // the URL will be generated dynamicly
      let fixed_link = str::replace(
        &str::replace(url, "{{current_version}}", current_version),
        "{{target}}",
        &target,
      );

      // we want JSON only
      let mut headers = HashMap::new();
      headers.insert("Accept".into(), "application/json".into());

      let resp = ClientBuilder::new()
        .build()?
        .send(
          HttpRequestBuilder::new("GET", &fixed_link)
            .headers(headers)
            // wait 20sec for the firewall
            .timeout(20),
        )
        .await;

      // If we got a success, we stop the loop
      // and we set our remote_release variable
      if let Ok(res) = resp {
        let res = res.read().await?;
        // got status code 2XX
        if StatusCode::from_u16(res.status).unwrap().is_success() {
          // if we got 204
          if StatusCode::NO_CONTENT.as_u16() == res.status {
            // return with `UpToDate` error
            // we should catch on the client
            return Err(Error::UpToDate);
          };
          // Convert the remote result to our local struct
          let built_release = RemoteRelease::from_release(&res.data, &target);
          // make sure all went well and the remote data is compatible
          // with what we need locally
          match built_release {
            Ok(release) => {
              last_error = None;
              remote_release = Some(release);
              break;
            }
            Err(err) => last_error = Some(err),
          }
        } // if status code is not 2XX we keep loopin' our urls
      }
    }

    // Last error is cleaned on success -- shouldn't be triggered if
    // we have a successful call
    if let Some(error) = last_error {
      return Err(Error::Network(error.to_string()));
    }

    // Extracted remote metadata
    let final_release = remote_release.ok_or_else(|| {
      Error::RemoteMetadata("Unable to extract update metadata from the remote server.".into())
    })?;

    // did the announced version is greated than our current one?
    let should_update =
      version::is_greater(current_version, &final_release.version).unwrap_or(false);

    // create our new updater
    Ok(Update {
      target,
      extract_path,
      should_update,
      version: final_release.version,
      date: final_release.date,
      current_version: self.current_version.to_owned(),
      download_url: final_release.download_url,
      body: final_release.body,
      signature: final_release.signature,
    })
  }
}

pub fn builder<'a>() -> UpdateBuilder<'a> {
  UpdateBuilder::new()
}

#[derive(Clone)]
pub struct Update {
  /// Update description
  pub body: Option<String>,
  /// Should we update or not
  pub should_update: bool,
  /// Version announced
  pub version: String,
  /// Running version
  pub current_version: String,
  /// Update publish date
  pub date: String,
  /// Target
  target: String,
  /// Extract path
  extract_path: PathBuf,
  /// Download URL announced
  download_url: String,
  /// Signature announced
  signature: Option<String>,
}

impl Update {
  // Download and install our update
  // @todo(lemarier): Split into download and install (two step) but need to be thread safe
  pub async fn download_and_install(&self, pub_key: Option<String>) -> Result {
    // download url for selected release
    let url = self.download_url.clone();
    // extract path
    let extract_path = self.extract_path.clone();

    // make sure we can install the update on linux
    // We fail here because later we can add more linux support
    // actually if we use APPIMAGE, our extract path should already
    // be set with our APPIMAGE env variable, we don't need to do
    // anythin with it yet
    #[cfg(target_os = "linux")]
    if env::var_os("APPIMAGE").is_none() {
      return Err(Error::UnsupportedPlatform);
    }

    // used  for temp file name
    // if we cant extract app name, we use unix epoch duration
    let current_time = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("Unable to get Unix Epoch")
      .subsec_nanos()
      .to_string();

    // get the current app name
    let bin_name = std::env::current_exe()
      .ok()
      .and_then(|pb| pb.file_name().map(|s| s.to_os_string()))
      .and_then(|s| s.into_string().ok())
      .unwrap_or_else(|| current_time.clone());

    // tmp dir for extraction
    let tmp_dir = tempfile::Builder::new()
      .prefix(&format!("{}_{}_download", bin_name, current_time))
      .tempdir()?;

    // tmp directories are used to create backup of current application
    // if something goes wrong, we can restore to previous state
    let tmp_archive_path = tmp_dir.path().join(detect_archive_in_url(&url));
    let mut tmp_archive = File::create(&tmp_archive_path)?;

    // set our headers
    let mut headers = HashMap::new();
    headers.insert("Accept".into(), "application/octet-stream".into());
    headers.insert("User-Agent".into(), "tauri/updater".into());

    // Create our request
    let resp = ClientBuilder::new()
      .build()?
      .send(
        HttpRequestBuilder::new("GET", &url)
          .headers(headers)
          // wait 20sec for the firewall
          .timeout(20),
      )
      .await?
      .bytes()
      .await?;

    // make sure it's success
    if !StatusCode::from_u16(resp.status).unwrap().is_success() {
      return Err(Error::Network(format!(
        "Download request failed with status: {}",
        resp.status
      )));
    }

    tmp_archive.write_all(&resp.data)?;

    // Validate signature ONLY if pubkey is available in tauri.conf.json
    if let Some(pub_key) = pub_key {
      // We need an announced signature by the server
      // if there is no signature, bail out.
      if let Some(signature) = self.signature.clone() {
        // we make sure the archive is valid and signed with the private key linked with the publickey
        verify_signature(&tmp_archive_path, signature, &pub_key)?;
      } else {
        // We have a public key inside our source file, but not announced by the server,
        // we assume this update is NOT valid.
        return Err(Error::PubkeyButNoSignature);
      }
    }
    // extract using tauri api inside a tmp path
    Extract::from_source(&tmp_archive_path).extract_into(tmp_dir.path())?;
    // Remove archive (not needed anymore)
    remove_file(&tmp_archive_path)?;
    // we copy the files depending of the operating system
    // we run the setup, appimage re-install or overwrite the
    // macos .app
    copy_files_and_run(tmp_dir, extract_path)?;
    // We are done!
    Ok(())
  }
}

// Linux (AppImage)

// ### Expected structure:
// ├── [AppName]_[version]_amd64.AppImage.tar.gz    # GZ generated by tauri-bundler
// │   └──[AppName]_[version]_amd64.AppImage        # Application AppImage
// └── ...

// We should have an AppImage already installed to be able to copy and install
// the extract_path is the current AppImage path
// tmp_dir is where our new AppImage is found
#[cfg(target_os = "linux")]
fn copy_files_and_run(tmp_dir: tempfile::TempDir, extract_path: PathBuf) -> Result {
  // we delete our current AppImage (we'll create a new one later)
  remove_file(&extract_path)?;

  // In our tempdir we expect 1 directory (should be the <app>.app)
  let paths = read_dir(&tmp_dir)?;

  for path in paths {
    let found_path = path?.path();
    // make sure it's our .AppImage
    if found_path.extension() == Some(OsStr::new("AppImage")) {
      // Simply overwrite our AppImage (we use the command)
      // because it prevent failing of bytes stream
      Command::new("mv")
        .arg("-f")
        .arg(&found_path)
        .arg(&extract_path)
        .status()?;

      // early finish we have everything we need here
      return Ok(());
    }
  }

  Ok(())
}

// Windows

// ### Expected structure:
// ├── [AppName]_[version]_x64.msi.zip          # ZIP generated by tauri-bundler
// │   └──[AppName]_[version]_x64.msi           # Application MSI
// └── ...

// ## MSI
// Update server can provide a MSI for Windows. (Generated with tauri-bundler from *Wix*)
// To replace current version of the application. In later version we'll offer
// incremental update to push specific binaries.

// ## EXE
// Update server can provide a custom EXE (installer) who can run any task.
#[cfg(target_os = "windows")]
#[allow(clippy::unnecessary_wraps)]
fn copy_files_and_run(tmp_dir: tempfile::TempDir, _extract_path: PathBuf) -> Result {
  use crate::api::file::Move;

  let paths = read_dir(&tmp_dir)?;
  // This consumes the TempDir without deleting directory on the filesystem,
  // meaning that the directory will no longer be automatically deleted.
  let tmp_path = tmp_dir.into_path();
  for path in paths {
    let found_path = path?.path();
    // we support 2 type of files exe & msi for now
    // If it's an `exe` we expect an installer not a runtime.
    if found_path.extension() == Some(OsStr::new("exe")) {
      // Run the EXE
      Command::new(found_path)
        .spawn()
        .expect("installer failed to start");

      exit(0);
    } else if found_path.extension() == Some(OsStr::new("msi")) {
      if let Some(bin_name) = std::env::current_exe()
        .ok()
        .and_then(|pb| pb.file_name().map(|s| s.to_os_string()))
        .and_then(|s| s.into_string().ok())
      {
        let product_name = bin_name.replace(".exe", "");

        // Check if there is a task that enables the updater to skip the UAC prompt
        let update_task_name = format!("Update {} - Skip UAC", product_name);
        if let Ok(status) = Command::new("schtasks")
          .arg("/QUERY")
          .arg("/TN")
          .arg(update_task_name.clone())
          .status()
        {
          if status.success() {
            // Rename the MSI to the match file name the Skip UAC task is expecting it to be
            let temp_msi = tmp_path.with_file_name(bin_name).with_extension("msi");
            Move::from_source(&found_path)
              .to_dest(&temp_msi)
              .expect("Unable to move update MSI");
            let exit_status = Command::new("schtasks")
              .arg("/RUN")
              .arg("/TN")
              .arg(update_task_name)
              .status()
              .expect("failed to start updater task");

            if exit_status.success() {
              // Successfully launched task that skips the UAC prompt
              exit(0);
            }
          }

          // Failed to run update task. Following UAC Path
        }
      }

      // restart should be handled by WIX as we exit the process
      Command::new("msiexec.exe")
        .arg("/i")
        .arg(found_path)
        // quiet basic UI with prompt at the end
        .arg("/qb+")
        .spawn()
        .expect("installer failed to start");

      exit(0);
    }
  }

  Ok(())
}

// Get the current app name in the path
// Example; `/Applications/updater-example.app/Contents/MacOS/updater-example`
// Should return; `updater-example.app`
#[cfg(target_os = "macos")]
fn macos_app_name_in_path(extract_path: &PathBuf) -> String {
  let components = extract_path.components();
  let app_name = components.last().unwrap();
  let app_name = app_name.as_os_str().to_str().unwrap();
  app_name.to_string()
}

// MacOS
// ### Expected structure:
// ├── [AppName]_[version]_x64.app.tar.gz       # GZ generated by tauri-bundler
// │   └──[AppName].app                         # Main application
// │      └── Contents                          # Application contents...
// │          └── ...
// └── ...
#[cfg(target_os = "macos")]
fn copy_files_and_run(tmp_dir: tempfile::TempDir, extract_path: PathBuf) -> Result {
  // In our tempdir we expect 1 directory (should be the <app>.app)
  let paths = read_dir(&tmp_dir)?;

  // current app name in /Applications/<app>.app
  let app_name = macos_app_name_in_path(&extract_path);

  for path in paths {
    let mut found_path = path?.path();
    // make sure it's our .app
    if found_path.extension() == Some(OsStr::new("app")) {
      let found_app_name = macos_app_name_in_path(&found_path);
      // make sure the app name in the archive matche the installed app name on path
      if found_app_name != app_name {
        // we need to replace the app name in the updater archive to match
        // installed app name
        let new_path = found_path.parent().unwrap().join(app_name);
        rename(&found_path, &new_path)?;

        found_path = new_path;
      }

      let sandbox_app_path = tempfile::Builder::new()
        .prefix("tauri_current_app_sandbox")
        .tempdir()?;

      // Replace the whole application to make sure the
      // code signature is following
      Move::from_source(&found_path)
        .replace_using_temp(sandbox_app_path.path())
        .to_dest(&extract_path)?;

      // early finish we have everything we need here
      return Ok(());
    }
  }

  Ok(())
}

/// Returns a target os
/// We do not use a helper function like the target_triple
/// from tauri-utils because this function return `None` if
/// the updater do not support the platform.
///
/// Available target: `linux, darwin, win32, win64`
pub fn get_updater_target() -> Option<String> {
  if cfg!(target_os = "linux") {
    Some("linux".into())
  } else if cfg!(target_os = "macos") {
    Some("darwin".into())
  } else if cfg!(target_os = "windows") {
    if cfg!(target_pointer_width = "32") {
      Some("win32".into())
    } else {
      Some("win64".into())
    }
  } else {
    None
  }
}

/// Get the extract_path from the provided executable_path
pub fn extract_path_from_executable(executable_path: &Path) -> PathBuf {
  // Return the path of the current executable by default
  // Example C:\Program Files\My App\
  let extract_path = executable_path
    .parent()
    .map(PathBuf::from)
    .expect("Can't determine extract path");

  // MacOS example binary is in /Applications/TestApp.app/Contents/MacOS/myApp
  // We need to get /Applications/<app>.app
  // todo(lemarier): Need a better way here
  // Maybe we could search for <*.app> to get the right path
  #[cfg(target_os = "macos")]
  if extract_path
    .display()
    .to_string()
    .contains("Contents/MacOS")
  {
    return extract_path
      .parent()
      .map(PathBuf::from)
      .expect("Unable to find the extract path")
      .parent()
      .map(PathBuf::from)
      .expect("Unable to find the extract path");
  }

  // We should use APPIMAGE exposed env variable
  // This is where our APPIMAGE should sit and should be replaced
  #[cfg(target_os = "linux")]
  if let Some(app_image_path) = env::var_os("APPIMAGE") {
    return PathBuf::from(app_image_path);
  }

  extract_path
}

// Return the archive type to save on disk
fn detect_archive_in_url(path: &str) -> String {
  path
    .split('/')
    .next_back()
    .unwrap_or(&default_archive_name_by_os())
    .to_string()
}

// Fallback archive name by os
// The main objective is to provide the right extension based on the target
// if we cant extract the archive type in the url we'll fallback to this value
fn default_archive_name_by_os() -> String {
  #[cfg(target_os = "windows")]
  {
    "update.zip".into()
  }

  #[cfg(not(target_os = "windows"))]
  {
    "update.tar.gz".into()
  }
}

// Convert base64 to string and prevent failing
fn base64_to_string(base64_string: &str) -> Result<String> {
  let decoded_string = &decode(base64_string.to_owned())?;
  let result = from_utf8(decoded_string)?.to_string();
  Ok(result)
}

// Validate signature
// need to be public because its been used
// by our tests in the bundler
pub fn verify_signature(
  archive_path: &Path,
  release_signature: String,
  pub_key: &str,
) -> Result<bool> {
  // we need to convert the pub key
  let pub_key_decoded = &base64_to_string(pub_key)?;
  let public_key = PublicKey::decode(pub_key_decoded)?;
  let signature_base64_decoded = base64_to_string(&release_signature)?;

  let signature = Signature::decode(&signature_base64_decoded)?;

  // We need to open the file and extract the datas to make sure its not corrupted
  let file_open = OpenOptions::new().read(true).open(&archive_path)?;

  let mut file_buff: BufReader<File> = BufReader::new(file_open);

  // read all bytes since EOF in the buffer
  let mut data = vec![];
  file_buff.read_to_end(&mut data)?;

  // Validate signature or bail out
  public_key.verify(&data, &signature)?;
  Ok(true)
}

#[cfg(test)]
mod test {
  use super::*;
  #[cfg(target_os = "macos")]
  use std::env::current_exe;
  #[cfg(target_os = "macos")]
  use std::fs::File;
  #[cfg(target_os = "macos")]
  use std::path::Path;

  macro_rules! block {
    ($e:expr) => {
      tokio_test::block_on($e)
    };
  }

  fn generate_sample_raw_json() -> String {
    r#"{
      "version": "v2.0.0",
      "notes": "Test version !",
      "pub_date": "2020-06-22T19:25:57Z",
      "platforms": {
        "darwin": {
          "signature": "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUldUTE5QWWxkQnlZOVJZVGdpKzJmRWZ0SkRvWS9TdFpqTU9xcm1mUmJSSG5OWVlwSklrWkN1SFpWbmh4SDlBcTU3SXpjbm0xMmRjRkphbkpVeGhGcTdrdzlrWGpGVWZQSWdzPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNTkyOTE1MDU3CWZpbGU6L1VzZXJzL3J1bm5lci9ydW5uZXJzLzIuMjYzLjAvd29yay90YXVyaS90YXVyaS90YXVyaS9leGFtcGxlcy9jb21tdW5pY2F0aW9uL3NyYy10YXVyaS90YXJnZXQvZGVidWcvYnVuZGxlL29zeC9hcHAuYXBwLnRhci5negp4ZHFlUkJTVnpGUXdDdEhydTE5TGgvRlVPeVhjTnM5RHdmaGx3c0ZPWjZXWnFwVDRNWEFSbUJTZ1ZkU1IwckJGdmlwSzJPd00zZEZFN2hJOFUvL1FDZz09Cg==",
          "url": "https://github.com/lemarier/tauri-test/releases/download/v1.0.0/app.app.tar.gz"
        },
        "linux": {
          "signature": "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUldUTE5QWWxkQnlZOWZSM29hTFNmUEdXMHRoOC81WDFFVVFRaXdWOUdXUUdwT0NlMldqdXkyaWVieXpoUmdZeXBJaXRqSm1YVmczNXdRL1Brc0tHb1NOTzhrL1hadFcxdmdnPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNTkyOTE3MzQzCWZpbGU6L2hvbWUvcnVubmVyL3dvcmsvdGF1cmkvdGF1cmkvdGF1cmkvZXhhbXBsZXMvY29tbXVuaWNhdGlvbi9zcmMtdGF1cmkvdGFyZ2V0L2RlYnVnL2J1bmRsZS9hcHBpbWFnZS9hcHAuQXBwSW1hZ2UudGFyLmd6CmRUTUM2bWxnbEtTbUhOZGtERUtaZnpUMG5qbVo5TGhtZWE1SFNWMk5OOENaVEZHcnAvVW0zc1A2ajJEbWZUbU0yalRHT0FYYjJNVTVHOHdTQlYwQkF3PT0K",
          "url": "https://github.com/lemarier/tauri-test/releases/download/v1.0.0/app.AppImage.tar.gz"
        },
        "win64": {
          "signature": "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUldUTE5QWWxkQnlZOVJHMWlvTzRUSlQzTHJOMm5waWpic0p0VVI2R0hUNGxhQVMxdzBPRndlbGpXQXJJakpTN0toRURtVzBkcm15R0VaNTJuS1lZRWdzMzZsWlNKUVAzZGdJPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNTkyOTE1NTIzCWZpbGU6RDpcYVx0YXVyaVx0YXVyaVx0YXVyaVxleGFtcGxlc1xjb21tdW5pY2F0aW9uXHNyYy10YXVyaVx0YXJnZXRcZGVidWdcYXBwLng2NC5tc2kuemlwCitXa1lQc3A2MCs1KzEwZnVhOGxyZ2dGMlZqbjBaVUplWEltYUdyZ255eUF6eVF1dldWZzFObStaVEQ3QU1RS1lzcjhDVU4wWFovQ1p1QjJXbW1YZUJ3PT0K",
          "url": "https://github.com/lemarier/tauri-test/releases/download/v1.0.0/app.x64.msi.zip"
        }
      }
    }"#.into()
  }

  fn generate_sample_platform_json(
    version: &str,
    public_signature: &str,
    download_url: &str,
  ) -> String {
    format!(
      r#"
        {{
          "name": "v{}",
          "notes": "This is the latest version! Once updated you shouldn't see this prompt.",
          "pub_date": "2020-06-25T14:14:19Z",
          "signature": "{}",
          "url": "{}"
        }}
      "#,
      version, public_signature, download_url
    )
  }

  fn generate_sample_bad_json() -> String {
    r#"{
      "version": "v0.0.3",
      "notes": "Blablaa",
      "date": "2020-02-20T15:41:00Z",
      "download_link": "https://github.com/lemarier/tauri-test/releases/download/v0.0.1/update3.tar.gz"
    }"#.into()
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn test_app_name_in_path() {
    let executable = extract_path_from_executable(Path::new(
      "/Applications/updater-example.app/Contents/MacOS/updater-example",
    ));
    let app_name = macos_app_name_in_path(&executable);
    assert!(executable.ends_with("updater-example.app"));
    assert_eq!(app_name, "updater-example.app".to_string());
  }

  #[test]
  fn simple_http_updater() {
    let _m = mockito::mock("GET", "/")
      .with_status(200)
      .with_header("content-type", "application/json")
      .with_body(generate_sample_raw_json())
      .create();

    let check_update = block!(builder()
      .current_version("0.0.0")
      .url(mockito::server_url())
      .build());

    assert!(check_update.is_ok());
    let updater = check_update.expect("Can't check update");

    assert!(updater.should_update);
  }

  #[test]
  fn simple_http_updater_raw_json() {
    let _m = mockito::mock("GET", "/")
      .with_status(200)
      .with_header("content-type", "application/json")
      .with_body(generate_sample_raw_json())
      .create();

    let check_update = block!(builder()
      .current_version("0.0.0")
      .url(mockito::server_url())
      .build());

    assert!(check_update.is_ok());
    let updater = check_update.expect("Can't check update");

    assert!(updater.should_update);
  }

  #[test]
  fn simple_http_updater_raw_json_win64() {
    let _m = mockito::mock("GET", "/")
      .with_status(200)
      .with_header("content-type", "application/json")
      .with_body(generate_sample_raw_json())
      .create();

    let check_update = block!(builder()
      .current_version("0.0.0")
      .target("win64")
      .url(mockito::server_url())
      .build());

    assert!(check_update.is_ok());
    let updater = check_update.expect("Can't check update");

    assert!(updater.should_update);
    assert_eq!(updater.version, "2.0.0");
    assert_eq!(updater.signature, Some("dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUldUTE5QWWxkQnlZOVJHMWlvTzRUSlQzTHJOMm5waWpic0p0VVI2R0hUNGxhQVMxdzBPRndlbGpXQXJJakpTN0toRURtVzBkcm15R0VaNTJuS1lZRWdzMzZsWlNKUVAzZGdJPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNTkyOTE1NTIzCWZpbGU6RDpcYVx0YXVyaVx0YXVyaVx0YXVyaVxleGFtcGxlc1xjb21tdW5pY2F0aW9uXHNyYy10YXVyaVx0YXJnZXRcZGVidWdcYXBwLng2NC5tc2kuemlwCitXa1lQc3A2MCs1KzEwZnVhOGxyZ2dGMlZqbjBaVUplWEltYUdyZ255eUF6eVF1dldWZzFObStaVEQ3QU1RS1lzcjhDVU4wWFovQ1p1QjJXbW1YZUJ3PT0K".into()));
    assert_eq!(
      updater.download_url,
      "https://github.com/lemarier/tauri-test/releases/download/v1.0.0/app.x64.msi.zip"
    );
  }

  #[test]
  fn simple_http_updater_raw_json_uptodate() {
    let _m = mockito::mock("GET", "/")
      .with_status(200)
      .with_header("content-type", "application/json")
      .with_body(generate_sample_raw_json())
      .create();

    let check_update = block!(builder()
      .current_version("10.0.0")
      .url(mockito::server_url())
      .build());

    assert!(check_update.is_ok());
    let updater = check_update.expect("Can't check update");

    assert!(!updater.should_update);
  }

  #[test]
  fn simple_http_updater_without_version() {
    let _m = mockito::mock("GET", "/darwin/1.0.0")
      .with_status(200)
      .with_header("content-type", "application/json")
      .with_body(generate_sample_platform_json(
        "2.0.0",
        "SampleTauriKey",
        "https://tauri.studio",
      ))
      .create();

    let check_update = block!(builder()
      .current_version("1.0.0")
      .url(format!(
        "{}/darwin/{{{{current_version}}}}",
        mockito::server_url()
      ))
      .build());

    assert!(check_update.is_ok());
    let updater = check_update.expect("Can't check update");

    assert!(updater.should_update);
  }

  #[test]
  fn http_updater_uptodate() {
    let _m = mockito::mock("GET", "/darwin/10.0.0")
      .with_status(200)
      .with_header("content-type", "application/json")
      .with_body(generate_sample_platform_json(
        "2.0.0",
        "SampleTauriKey",
        "https://tauri.studio",
      ))
      .create();

    let check_update = block!(builder()
      .current_version("10.0.0")
      .url(format!(
        "{}/darwin/{{{{current_version}}}}",
        mockito::server_url()
      ))
      .build());

    assert!(check_update.is_ok());
    let updater = check_update.expect("Can't check update");

    assert!(!updater.should_update);
  }

  #[test]
  fn http_updater_fallback_urls() {
    let _m = mockito::mock("GET", "/")
      .with_status(200)
      .with_header("content-type", "application/json")
      .with_body(generate_sample_raw_json())
      .create();

    let check_update = block!(builder()
      .url("http://badurl.www.tld/1".into())
      .url(mockito::server_url())
      .current_version("0.0.1")
      .build());

    assert!(check_update.is_ok());
    let updater = check_update.expect("Can't check remote update");

    assert!(updater.should_update);
  }

  #[test]
  fn http_updater_fallback_urls_withs_array() {
    let _m = mockito::mock("GET", "/")
      .with_status(200)
      .with_header("content-type", "application/json")
      .with_body(generate_sample_raw_json())
      .create();

    let check_update = block!(builder()
      .urls(&["http://badurl.www.tld/1".into(), mockito::server_url(),])
      .current_version("0.0.1")
      .build());

    assert!(check_update.is_ok());
    let updater = check_update.expect("Can't check remote update");

    assert!(updater.should_update);
  }

  #[test]
  fn http_updater_missing_remote_data() {
    let _m = mockito::mock("GET", "/")
      .with_status(200)
      .with_header("content-type", "application/json")
      .with_body(generate_sample_bad_json())
      .create();

    let check_update = block!(builder()
      .url(mockito::server_url())
      .current_version("0.0.1")
      .build());

    assert!(check_update.is_err());
  }

  // run complete process on mac only for now as we don't have
  // server (api) that we can use to test
  #[cfg(target_os = "macos")]
  #[test]
  fn http_updater_complete_process() {
    let good_archive_url = format!("{}/archive.tar.gz", mockito::server_url());

    let mut signature_file = File::open("./test/updater/fixture/archives/archive.tar.gz.sig")
      .expect("Unable to open signature");
    let mut signature = String::new();
    signature_file
      .read_to_string(&mut signature)
      .expect("Unable to read signature as string");

    let mut pubkey_file = File::open("./test/updater/fixture/good_signature/update.key.pub")
      .expect("Unable to open pubkey");
    let mut pubkey = String::new();
    pubkey_file
      .read_to_string(&mut pubkey)
      .expect("Unable to read signature as string");

    // add sample file
    let _m = mockito::mock("GET", "/archive.tar.gz")
      .with_status(200)
      .with_header("content-type", "application/octet-stream")
      .with_body_from_file("./test/updater/fixture/archives/archive.tar.gz")
      .create();

    // sample mock for update file
    let _m = mockito::mock("GET", "/")
      .with_status(200)
      .with_header("content-type", "application/json")
      .with_body(generate_sample_platform_json(
        "2.0.1",
        signature.as_ref(),
        good_archive_url.as_ref(),
      ))
      .create();

    // Build a tmpdir so we can test our extraction inside
    // We dont want to overwrite our current executable or the directory
    // Otherwise tests are failing...
    let executable_path = current_exe().expect("Can't extract executable path");
    let parent_path = executable_path
      .parent()
      .expect("Can't find the parent path");

    let tmp_dir = tempfile::Builder::new()
      .prefix("tauri_updater_test")
      .tempdir_in(parent_path);

    assert!(tmp_dir.is_ok());
    let tmp_dir_unwrap = tmp_dir.expect("Can't find tmp_dir");
    let tmp_dir_path = tmp_dir_unwrap.path();

    // configure the updater
    let check_update = block!(builder()
      .url(mockito::server_url())
      // It should represent the executable path, that's why we add my_app.exe in our
      // test path -- in production you shouldn't have to provide it
      .executable_path(&tmp_dir_path.join("my_app.exe"))
      // make sure we force an update
      .current_version("1.0.0")
      .build());

    // make sure the process worked
    assert!(check_update.is_ok());

    // unwrap our results
    let updater = check_update.expect("Can't check remote update");

    // make sure we need to update
    assert!(updater.should_update);
    // make sure we can read announced version
    assert_eq!(updater.version, "2.0.1");

    // download, install and validate signature
    let install_process = block!(updater.download_and_install(Some(pubkey)));
    assert!(install_process.is_ok());

    // make sure the extraction went well (it should have skipped the main app.app folder)
    // as we can't extract in /Applications directly
    let bin_file = tmp_dir_path.join("Contents").join("MacOS").join("app");
    let bin_file_exist = Path::new(&bin_file).exists();
    assert!(bin_file_exist);
  }
}
