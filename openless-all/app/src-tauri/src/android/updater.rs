//! Android in-app updater: manifest fetch, minisign verify, system APK install.

#[cfg(target_os = "android")]
mod android_impl {
    use std::path::PathBuf;

    use minisign_verify::{PublicKey, Signature};
    use serde::Deserialize;
    use tauri::{AppHandle, Emitter};

    use crate::commands::{
        fetch_latest_beta_release, parse_latest_beta_from_atom, AppUpdateMetadata,
    };
    use crate::net;
    use crate::types::UpdateChannel;

    const PUBKEY_B64: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDFERUFBODAzNTY0QzMyM0YKUldRL01reFdBNmpxSGE1K0JadlpONXNWTzhJcGZCRGxjUVdIWExNNFJpeUNsSGZwazdlQThhemkK";
    const MIRROR_BASE: &str = "https://fastgit.cc/https://github.com/appergb/openless";
    const DIRECT_BASE: &str = "https://github.com/appergb/openless";

    #[derive(Debug, Deserialize)]
    struct UpdaterManifest {
        version: String,
        #[serde(default)]
        pub_date: Option<String>,
        url: String,
        signature: String,
        #[serde(default)]
        notes: Option<String>,
    }

    #[derive(Clone, serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct AndroidUpdateProgress {
        pub downloaded: u64,
        pub content_length: Option<u64>,
        pub phase: String,
    }

    fn device_arch() -> Result<&'static str, String> {
        crate::android::jni::android::with_android_env(|env, _context| {
            let abis_obj = env
                .get_static_field(
                    "android/os/Build",
                    "SUPPORTED_ABIS",
                    "[Ljava/lang/String;",
                )
                .and_then(|v| v.l())
                .map_err(|e| format!("read SUPPORTED_ABIS: {e}"))?;
            let abis_array = jni::objects::JObjectArray::from(abis_obj);
            let len = env
                .get_array_length(&abis_array)
                .map_err(|e| format!("SUPPORTED_ABIS length: {e}"))?;
            if len == 0 {
                return Err("SUPPORTED_ABIS is empty".to_string());
            }
            let first = env
                .get_object_array_element(&abis_array, 0)
                .map_err(|e| format!("SUPPORTED_ABIS[0]: {e}"))?;
            let abi = env
                .get_string(&jni::objects::JString::from(first))
                .map_err(|e| format!("read abi string: {e}"))?
                .to_string_lossy()
                .into_owned();
            Ok(map_abi_to_arch(&abi))
        })
    }

    fn map_abi_to_arch(abi: &str) -> &'static str {
        match abi {
            "arm64-v8a" => "aarch64",
            "armeabi-v7a" => "armv7",
            "x86" => "i686",
            "x86_64" => "x86_64",
            _ => "aarch64",
        }
    }

    fn version_is_newer(remote: &str, current: &str) -> bool {
        fn parts(v: &str) -> Vec<u32> {
            v.split(|c| c == '.' || c == '-')
                .filter_map(|p| p.parse().ok())
                .collect()
        }
        let remote_parts = parts(remote);
        let current_parts = parts(current);
        let max = remote_parts.len().max(current_parts.len());
        for i in 0..max {
            let r = remote_parts.get(i).copied().unwrap_or(0);
            let c = current_parts.get(i).copied().unwrap_or(0);
            if r > c {
                return true;
            }
            if r < c {
                return false;
            }
        }
        false
    }

    async fn fetch_manifest(url: &str) -> Result<UpdaterManifest, String> {
        let resp = net::send_with_retry(|| {
            net::http()
                .get(url)
                .timeout(std::time::Duration::from_secs(15))
        })
        .await
        .map_err(|e| format!("fetch manifest: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("manifest status {}", resp.status()));
        }
        resp.json::<UpdaterManifest>()
            .await
            .map_err(|e| format!("parse manifest: {e}"))
    }

    async fn resolve_stable_manifest_urls(arch: &str) -> Vec<String> {
        vec![
            format!("{MIRROR_BASE}/releases/latest/download/latest-android-{arch}-mirror.json"),
            format!("{DIRECT_BASE}/releases/latest/download/latest-android-{arch}.json"),
        ]
    }

    async fn resolve_beta_manifest_urls(arch: &str) -> Result<Vec<String>, String> {
        let Some(latest) = fetch_latest_beta_release().await? else {
            return Err("尚未发布过 Beta 版本".to_string());
        };
        let tag = latest.tag_name;
        Ok(vec![
            format!("{MIRROR_BASE}/releases/download/{tag}/latest-android-{arch}-beta-mirror.json"),
            format!("{DIRECT_BASE}/releases/download/{tag}/latest-android-{arch}-beta.json"),
        ])
    }

    pub async fn check_update(channel: UpdateChannel) -> Result<Option<AppUpdateMetadata>, String> {
        let arch = device_arch()?;
        let urls = match channel {
            UpdateChannel::Stable => resolve_stable_manifest_urls(arch).await,
            UpdateChannel::Beta => resolve_beta_manifest_urls(arch).await?,
        };
        let current = env!("CARGO_PKG_VERSION").to_string();
        let mut last_err = String::new();
        for url in urls {
            match fetch_manifest(&url).await {
                Ok(manifest) => {
                    if !version_is_newer(&manifest.version, &current) {
                        return Ok(None);
                    }
                    let raw_json = serde_json::json!({
                        "version": manifest.version,
                        "url": manifest.url,
                        "signature": manifest.signature,
                        "pubDate": manifest.pub_date,
                        "notes": manifest.notes,
                    });
                    return Ok(Some(AppUpdateMetadata {
                        rid: 0,
                        current_version: current,
                        version: manifest.version,
                        date: manifest.pub_date,
                        body: manifest.notes,
                        raw_json,
                    }));
                }
                Err(err) => last_err = err,
            }
        }
        Err(if last_err.is_empty() {
            "无法获取更新清单".to_string()
        } else {
            last_err
        })
    }

    fn verify_signature(apk_bytes: &[u8], signature_b64: &str) -> Result<(), String> {
        let public_key =
            PublicKey::from_base64(PUBKEY_B64).map_err(|e| format!("parse updater pubkey: {e}"))?;
        let signature =
            Signature::decode(signature_b64.trim()).map_err(|e| format!("decode signature: {e}"))?;
        public_key
            .verify(apk_bytes, &signature, false)
            .map_err(|e| format!("signature verify failed: {e}"))?;
        Ok(())
    }

    fn updates_cache_dir() -> Result<PathBuf, String> {
        crate::android::jni::android::with_android_env(|env, context| {
            let cache = env
                .call_method(context, "getCacheDir", "()Ljava/io/File;", &[])
                .and_then(|value| value.l())
                .map_err(|e| format!("getCacheDir: {e}"))?;
            let path = env
                .call_method(&cache, "getAbsolutePath", "()Ljava/lang/String;", &[])
                .and_then(|value| value.l())
                .map_err(|e| format!("cache path: {e}"))?;
            let text = env
                .get_string(&jni::objects::JString::from(path))
                .map_err(|e| format!("read cache path: {e}"))?
                .to_string_lossy()
                .into_owned();
            Ok(PathBuf::from(text).join("updates"))
        })
    }

    pub async fn download_and_install(
        app: AppHandle,
        url: String,
        signature: String,
        version: String,
    ) -> Result<(), String> {
        let cache_dir = updates_cache_dir()?;
        std::fs::create_dir_all(&cache_dir).map_err(|e| format!("create cache dir: {e}"))?;
        let dest = cache_dir.join(format!("OpenLess_{version}.apk"));

        let resp = net::send_with_retry(|| net::http().get(&url))
            .await
            .map_err(|e| format!("download apk: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("download status {}", resp.status()));
        }
        let total = resp.content_length();
        // 安全：防止无限流耗尽内存。200 MB 远超任何实际 APK 大小（当前约 50 MB）。
        const MAX_APK_BYTES: u64 = 200 * 1024 * 1024;
        if let Some(len) = total {
            if len > MAX_APK_BYTES {
                return Err(format!(
                    "APK 声明大小 {len} 字节超过上限 {MAX_APK_BYTES}，拒绝下载"
                ));
            }
        }
        let mut downloaded: u64 = 0;
        let mut bytes = Vec::new();
        let mut stream = resp.bytes_stream();
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("download chunk: {e}"))?;
            downloaded += chunk.len() as u64;
            if downloaded > MAX_APK_BYTES {
                return Err(format!(
                    "下载字节数 {downloaded} 超过上限 {MAX_APK_BYTES}，已终止"
                ));
            }
            bytes.extend_from_slice(&chunk);
            let _ = app.emit(
                "android-update:progress",
                AndroidUpdateProgress {
                    downloaded,
                    content_length: total,
                    phase: "downloading".to_string(),
                },
            );
        }

        verify_signature(&bytes, &signature)?;
        std::fs::write(&dest, &bytes).map_err(|e| format!("write apk: {e}"))?;

        let _ = app.emit(
            "android-update:progress",
            AndroidUpdateProgress {
                downloaded,
                content_length: total,
                phase: "installing".to_string(),
            },
        );

        let path = dest
            .to_str()
            .ok_or_else(|| "apk path is not UTF-8".to_string())?
            .to_string();
        crate::android::jni::android::with_android_env(|env, context| {
            let path_obj = crate::android::jni::android::jobject_str(env, &path)?;
            crate::android::jni::android::install_apk_from_path(env, context, &path_obj)
        })?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn parse_beta_atom(body: &str) -> Option<crate::commands::LatestBetaRelease> {
        parse_latest_beta_from_atom(body)
    }
}

#[cfg(target_os = "android")]
pub use android_impl::*;
