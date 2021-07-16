// Copyright 2019-2021 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use kuchiki::traits::*;
use proc_macro2::TokenStream;
use quote::{quote, ToTokens, TokenStreamExt};
use regex::RegexSet;
use std::{
  collections::HashMap,
  ffi::OsStr,
  fs::File,
  path::{Path, PathBuf},
};
use tauri_utils::{
  assets::AssetKey,
  html::{inject_csp, inject_invoke_key_token},
};
use thiserror::Error;
use walkdir::WalkDir;

/// The subdirectory inside the target directory we want to place assets.
const TARGET_PATH: &str = "tauri-codegen-assets";

/// The minimum size needed for the hasher to use multiple threads.
const MULTI_HASH_SIZE_LIMIT: usize = 131_072; // 128KiB

/// (key, (original filepath, compressed bytes))
type Asset = (AssetKey, (PathBuf, PathBuf));

/// All possible errors while reading and compressing an [`EmbeddedAssets`] directory
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EmbeddedAssetsError {
  #[error("failed to read asset at {path} because {error}")]
  AssetRead {
    path: PathBuf,
    error: std::io::Error,
  },

  #[error("failed to write asset from {path} to Vec<u8> because {error}")]
  AssetWrite {
    path: PathBuf,
    error: std::io::Error,
  },

  #[error("invalid prefix {prefix} used while including path {path}")]
  PrefixInvalid { prefix: PathBuf, path: PathBuf },

  #[error("failed to walk directory {path} because {error}")]
  Walkdir {
    path: PathBuf,
    error: walkdir::Error,
  },

  #[error("OUT_DIR env var is not set, do you have a build script?")]
  OutDir,
}

/// Represent a directory of assets that are compressed and embedded.
///
/// This is the compile time generation of [`tauri_utils::assets::Assets`] from a directory. Assets
/// from the directory are added as compiler dependencies by dummy including the original,
/// uncompressed assets.
///
/// The assets are compressed during this runtime, and can only be represented as a [`TokenStream`]
/// through [`ToTokens`]. The generated code is meant to be injected into an application to include
/// the compressed assets in that application's binary.
#[derive(Default)]
pub struct EmbeddedAssets(HashMap<AssetKey, (PathBuf, PathBuf)>);

/// Options used to embed assets.
#[derive(Default)]
pub struct AssetOptions {
  csp: Option<String>,
}

impl AssetOptions {
  /// Creates the default asset options.
  pub fn new() -> Self {
    Self::default()
  }

  /// Sets the content security policy to add to HTML files.
  pub fn csp(mut self, csp: String) -> Self {
    self.csp.replace(csp);
    self
  }
}

impl EmbeddedAssets {
  /// Compress a directory of assets, ready to be generated into a [`tauri_utils::assets::Assets`].
  pub fn new(path: &Path, options: AssetOptions) -> Result<Self, EmbeddedAssetsError> {
    WalkDir::new(&path)
      .follow_links(true)
      .into_iter()
      .filter_map(|entry| match entry {
        // we only serve files, not directory listings
        Ok(entry) if entry.file_type().is_dir() => None,

        // compress all files encountered
        Ok(entry) => Some(Self::compress_file(path, entry.path(), &options)),

        // pass down error through filter to fail when encountering any error
        Err(error) => Some(Err(EmbeddedAssetsError::Walkdir {
          path: path.to_owned(),
          error,
        })),
      })
      .collect::<Result<_, _>>()
      .map(Self)
  }

  /// Compress a list of files and directories.
  pub fn load_paths(
    paths: Vec<PathBuf>,
    options: AssetOptions,
  ) -> Result<Self, EmbeddedAssetsError> {
    Ok(Self(
      paths
        .iter()
        .map(|path| {
          let is_file = path.is_file();
          WalkDir::new(&path)
            .follow_links(true)
            .into_iter()
            .filter_map(|entry| {
              match entry {
                // we only serve files, not directory listings
                Ok(entry) if entry.file_type().is_dir() => None,

                // compress all files encountered
                Ok(entry) => Some(Self::compress_file(
                  if is_file {
                    path.parent().unwrap()
                  } else {
                    path
                  },
                  entry.path(),
                  &options,
                )),

                // pass down error through filter to fail when encountering any error
                Err(error) => Some(Err(EmbeddedAssetsError::Walkdir {
                  path: path.to_path_buf(),
                  error,
                })),
              }
            })
            .collect::<Result<Vec<Asset>, _>>()
        })
        .flatten()
        .flatten()
        .collect::<_>(),
    ))
  }

  /// Use highest compression level for release, the fastest one for everything else
  fn compression_level() -> i32 {
    let levels = zstd::compression_level_range();
    if cfg!(debug_assertions) {
      *levels.start()
    } else {
      *levels.end()
    }
  }

  /// Compress a file and spit out the information in a [`HashMap`] friendly form.
  fn compress_file(
    prefix: &Path,
    path: &Path,
    options: &AssetOptions,
  ) -> Result<Asset, EmbeddedAssetsError> {
    let mut input = std::fs::read(path).map_err(|error| EmbeddedAssetsError::AssetRead {
      path: path.to_owned(),
      error,
    })?;
    if path.extension() == Some(OsStr::new("html")) {
      let mut document = kuchiki::parse_html().one(String::from_utf8_lossy(&input).into_owned());
      if let Some(csp) = &options.csp {
        inject_csp(&mut document, csp);
      }
      inject_invoke_key_token(&mut document);
      input = document.to_string().as_bytes().to_vec();
    } else {
      let is_javascript = ["js", "cjs", "mjs"]
        .iter()
        .any(|e| path.extension() == Some(OsStr::new(e)));
      if is_javascript {
        let js = String::from_utf8_lossy(&input).into_owned();
        input = if RegexSet::new(&[
          // import keywords
          "import\\{",
          "import \\{",
          "import\\*",
          "import \\*",
          "import (\"|');?$",
          "import\\(",
          "import (.|\n)+ from (\"|')([A-Za-z\\-]+)(\"|')",
          // export keywords
          "export\\{",
          "export \\{",
          "export\\*",
          "export \\*",
          "export (default|class|let|const|function)",
        ])
        .unwrap()
        .is_match(&js)
        {
          format!(
            r#"
              const __TAURI_INVOKE_KEY__ = __TAURI__INVOKE_KEY_TOKEN__;
              {}
            "#,
            js
          )
          .as_bytes()
          .to_vec()
        } else {
          format!(
            r#"(function () {{
              const __TAURI_INVOKE_KEY__ = __TAURI__INVOKE_KEY_TOKEN__;
              {}
            }})()"#,
            js
          )
          .as_bytes()
          .to_vec()
        };
      }
    }

    // we must canonicalize the base of our paths to allow long paths on windows
    let out_dir = std::env::var("OUT_DIR")
      .map_err(|_| EmbeddedAssetsError::OutDir)
      .map(PathBuf::from)
      .and_then(|p| p.canonicalize().map_err(|_| EmbeddedAssetsError::OutDir))
      .map(|p| p.join(TARGET_PATH))?;

    // make sure that our output directory is created
    std::fs::create_dir_all(&out_dir).map_err(|_| EmbeddedAssetsError::OutDir)?;

    // get a hash of the input - allows for caching existing files
    let hash = {
      let mut hasher = blake3::Hasher::new();
      if input.len() < MULTI_HASH_SIZE_LIMIT {
        hasher.update(&input);
      } else {
        hasher.update_with_join::<blake3::join::RayonJoin>(&input);
      }
      hasher.finalize().to_hex()
    };

    // use the content hash to determine filename, keep extensions that exist
    let out_path = if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
      out_dir.join(format!("{}.{}", hash, ext))
    } else {
      out_dir.join(hash.to_string())
    };

    // only compress and write to the file if it doesn't already exist.
    if !out_path.exists() {
      let out_file = File::create(&out_path).map_err(|error| EmbeddedAssetsError::AssetWrite {
        path: out_path.clone(),
        error,
      })?;

      // entirely write input to the output file path with compression
      zstd::stream::copy_encode(&*input, out_file, Self::compression_level()).map_err(|error| {
        EmbeddedAssetsError::AssetWrite {
          path: path.to_owned(),
          error,
        }
      })?;
    }

    // get a key to the asset path without the asset directory prefix
    let key = path
      .strip_prefix(prefix)
      .map(AssetKey::from) // format the path for use in assets
      .map_err(|_| EmbeddedAssetsError::PrefixInvalid {
        prefix: prefix.to_owned(),
        path: path.to_owned(),
      })?;

    Ok((key, (path.into(), out_path)))
  }
}

impl ToTokens for EmbeddedAssets {
  fn to_tokens(&self, tokens: &mut TokenStream) {
    let mut map = TokenStream::new();
    for (key, (input, output)) in &self.0 {
      let key: &str = key.as_ref();
      let input = input.display().to_string();
      let output = output.display().to_string();

      // add original asset as a compiler dependency, rely on dead code elimination to clean it up
      map.append_all(quote!(#key => {
        const _: &[u8] = include_bytes!(#input);
        include_bytes!(#output)
      },));
    }

    // we expect phf related items to be in path when generating the path code
    tokens.append_all(quote! {{
        use ::tauri::api::assets::{EmbeddedAssets, phf, phf::phf_map};
        EmbeddedAssets::from_zstd(phf_map! { #map })
    }});
  }
}
