use std::error::Error;

#[cfg(feature = "naist-jdic")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=prebuilt.json");
    println!("cargo:rerun-if-changed=build.json");
    println!("cargo:rerun-if-env-changed=JPREPROCESS_LOCAL_DICT");

    if std::env::var("DOCS_RS").is_ok() {
        // Skip building the dictionary when building docs.rs
        return Ok(());
    } else {
        fetch_dictionary::download(false).await
    }
}

#[cfg(not(feature = "naist-jdic"))]
fn main() -> Result<(), Box<dyn Error>> {
    Ok(())
}

#[cfg(feature = "naist-jdic")]
mod fetch_dictionary {
    use serde::{Deserialize, Serialize};
    use std::{
        error::Error,
        path::{Path, PathBuf},
    };

    pub async fn download(force_build: bool) -> Result<(), Box<dyn Error>> {
        let client = reqwest::ClientBuilder::new()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(concat!(
                "jpreprocess-naist-jdic/",
                env!("CARGO_PKG_VERSION"),
            ))
            .build()?;

        let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let work_dir = out_dir.join("work");
        let dict_dir = out_dir.join("naist-jdic");

        // 清理本 OUT_DIR 里上一次 build script 运行留下的词典/work 目录。
        // rerun-if-changed 文件（prebuilt.json、build.rs）变更会触发重跑，
        // 此时 OUT_DIR 不变，若不清干净，后续 rename/copy 会撞上非空目录
        // （DirectoryNotEmpty）。
        let _ = std::fs::remove_dir_all(&dict_dir);
        let _ = std::fs::remove_dir_all(&work_dir);

        println!(
            "cargo::rustc-env=JPREPROCESS_WORKDIR={}",
            dict_dir.display()
        );

        // LingChat local TTS workaround: prefer a locally-provided dictionary so
        // builds on slow/unstable networks (GitHub 30s timeout) don't fail.
        // JPREPROCESS_LOCAL_DICT may point to:
        //   - an extracted naist-jdic dir (the dir itself or a parent containing
        //     a `naist-jdic` subdir, identified by metadata.json), or
        //   - the naist-jdic-jpreprocess.tar.gz tarball (MD5-verified against
        //     prebuilt.json like the network download below).
        if let Some(local) = std::env::var_os("JPREPROCESS_LOCAL_DICT") {
            let p = PathBuf::from(local);
            for candidate in [p.clone(), p.join("naist-jdic")] {
                if candidate.join("metadata.json").is_file() {
                    println!(
                        "cargo:warning=naist-jdic: using local dictionary dir {}",
                        candidate.display()
                    );
                    copy_dir_all(&candidate, &dict_dir)?;
                    return Ok(());
                }
            }
            if p.is_file() {
                let prebuilt = {
                    let config_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("prebuilt.json");
                    let config_data = std::fs::read_to_string(config_path)?;
                    serde_json::from_str::<FetchConfig>(&config_data)?
                };
                let bytes = std::fs::read(&p)?;
                let mut context = md5::Context::new();
                context.consume(&bytes);
                let hash = format!("{:x}", context.finalize());
                if hash == prebuilt.digest {
                    println!(
                        "cargo:warning=naist-jdic: using local dictionary tarball {} (MD5 verified against upstream)",
                        p.display()
                    );
                } else {
                    // 可能是用户自己从解压目录重新打包的词典（顶层 naist-jdic/ 结构）。
                    // gzip/tar 解压过程已保证完整性，MD5 与上游不一致只做提示，不阻断。
                    println!(
                        "cargo:warning=naist-jdic: using local dictionary tarball {} (MD5={} differs from upstream {}; not verifying, relying on gzip/tar integrity)",
                        p.display(),
                        hash,
                        prebuilt.digest
                    );
                }
                let tmp = work_dir.join("naist-jdic-prebuilt-local");
                let file = std::fs::File::open(&p)?;
                let tar = flate2::read::GzDecoder::new(file);
                let mut archive = tar::Archive::new(tar);
                archive.unpack(&tmp)?;
                // Same extraction shape as download_prebuilt: the tarball holds a
                // single top-level dir that becomes OUT_DIR/naist-jdic.
                let name = std::fs::read_dir(&tmp)?
                    .next()
                    .ok_or("Empty naist-jdic tarball")??
                    .file_name();
                std::fs::rename(tmp.join(name), &dict_dir)?;
                return Ok(());
            }
            println!(
                "cargo:warning=naist-jdic: JPREPROCESS_LOCAL_DICT={} not found, falling back to network download",
                p.display()
            );
        }

        if !force_build {
            match download_prebuilt(&client, &work_dir, &dict_dir).await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    println!(
                    "Failed to download prebuilt naist-jdic, falling back to building from source: {}",
                    e
                );
                }
            }
        }

        println!("Downloading and building naist-jdic from source...");

        let build = {
            let config_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("build.json");
            let config_data = std::fs::read_to_string(config_path)?;
            serde_json::from_str::<BuildConfig>(&config_data)?
        };

        build.build(&client, &work_dir, &dict_dir).await?;

        Ok(())
    }

    fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?;
            } else {
                std::fs::copy(entry.path(), dst.join(entry.file_name()))?;
            }
        }
        Ok(())
    }

    async fn download_prebuilt(
        client: &reqwest::Client,
        work_dir: &Path,
        out_dir: &Path,
    ) -> Result<(), Box<dyn Error>> {
        let prebuilt = {
            let config_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("prebuilt.json");
            let config_data = std::fs::read_to_string(config_path)?;
            serde_json::from_str::<FetchConfig>(&config_data)?
        };

        println!("Attempting to download prebuilt naist-jdic...");
        let prebuilt_download_dir = work_dir.join("naist-jdic-prebuilt");
        prebuilt.fetch(client, &prebuilt_download_dir).await?;

        println!("Successfully downloaded prebuilt naist-jdic.");

        let prebuilt_name = std::fs::read_dir(&prebuilt_download_dir)?
            .next()
            .ok_or("No directory found in prebuilt download dir")??
            .file_name();
        let prebuilt_dir = prebuilt_download_dir.join(prebuilt_name);
        std::fs::rename(&prebuilt_dir, out_dir)?;

        Ok(())
    }

    /// Configuration for building the dictionary from source (fallback)
    #[derive(Clone, Serialize, Deserialize)]
    struct BuildConfig {
        src: FetchConfig,
        metadata: lindera_dictionary::dictionary::metadata::Metadata,
    }

    impl BuildConfig {
        async fn build(
            &self,
            client: &reqwest::Client,
            work_dir: &Path,
            out_dir: &Path,
        ) -> Result<(), Box<dyn Error>> {
            let src_download_dir = work_dir.join("src");
            self.src.fetch(client, &src_download_dir).await?;

            let src_name = std::fs::read_dir(&src_download_dir)?
                .next()
                .ok_or("No directory found in source download dir")??
                .file_name();
            let src_dir = src_download_dir.join(src_name);

            jpreprocess_dictionary::dictionary::to_dict::JPreprocessDictionaryBuilder::new(
                self.metadata.clone(),
            )
            .build_dictionary(&src_dir, out_dir)?;

            Ok(())
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct FetchConfig {
        url: String,
        digest: String,
    }

    impl FetchConfig {
        async fn fetch(&self, client: &reqwest::Client, path: &Path) -> Result<(), Box<dyn Error>> {
            let response = client.get(&self.url).send().await?;
            let bytes = response.bytes().await?;

            let mut context = md5::Context::new();
            context.consume(&bytes);
            let digest = context.finalize();
            let hash = format!("{:x}", digest);
            if hash != self.digest {
                return Err(Box::new(std::io::Error::other(format!(
                    "MD5 hash mismatch for prebuilt dictionary: expected {}, got {}",
                    self.digest, hash
                ))));
            }

            let tar = flate2::read::GzDecoder::new(&bytes[..]);
            let mut archive = tar::Archive::new(tar);
            archive.unpack(path)?;

            Ok(())
        }
    }
}
