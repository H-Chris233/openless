fn main() {
    #[cfg(target_os = "windows")]
    link_windows_common_controls_v6_manifest_dependency();

    // build.rs 的 `#[cfg(target_os)]` 判断的是构建脚本主机，不是 Cargo 的目标平台。
    // 直接解析 TARGET，明确排除所有 Android triple，避免 Linux 主机交叉编译时
    // 把 qwen-asr C 后端误编进 Android。
    let target = std::env::var("TARGET").unwrap_or_default();
    let target_os = if target.ends_with("-android") {
        "android"
    } else if target.ends_with("-apple-darwin") {
        "macos"
    } else if target.contains("-linux-") {
        "linux"
    } else {
        ""
    };
    println!("cargo:warning=OpenLess build target={target}, target_os={target_os}");
    if matches!(target_os, "macos" | "linux") {
        build_qwen_asr(target_os);
    }

    if target_os == "android" {
        link_android_cpp_runtime();
    }

    tauri_build::build();
}

/// cpal → oboe → oboe-sys 会编译 C++；最终 cdylib 需显式链接 NDK libc++。
fn link_android_cpp_runtime() {
    // oboe-ext 已部分静态链入 libc++；补链 c++abi 提供 __cxa_pure_virtual 等 ABI 符号。
    println!("cargo:rustc-link-lib=c++_static");
    println!("cargo:rustc-link-lib=c++abi");
}

#[cfg(target_os = "windows")]
fn link_windows_common_controls_v6_manifest_dependency() {
    let mut source_path = std::path::PathBuf::from(
        std::env::var_os("OUT_DIR").expect("OUT_DIR must be set by Cargo"),
    );
    source_path.push("common-controls-v6-manifest-dependency.c");
    std::fs::write(
        &source_path,
        r#"#pragma comment(linker, "/manifestdependency:\"type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'\"")
int openless_common_controls_v6_manifest_dependency_anchor = 0;
"#,
    )
    .expect("write common controls manifest dependency source");
    cc::Build::new()
        .file(&source_path)
        .compile("openless_common_controls_v6_manifest_dependency");
    println!(
        "cargo:rustc-link-arg=/INCLUDE:openless_common_controls_v6_manifest_dependency_anchor"
    );
}

/// 编译 vendored Open-Less/qwen-asr 的 C 源（macOS/Linux）。
///
/// 上游 Makefile `make blas` 等价配置：BLAS 加速通过 Accelerate framework，
/// `USE_BLAS` + `ACCELERATE_NEW_LAPACK` 是必要宏。
/// `-march=native` 这里**不**用——分发二进制要可移植，cc crate 在 release 下
/// 默认带 `-O2`，加上 `-O3` 提一档；NEON/AVX 在源码里有 `#ifdef` 自动分派。
fn build_qwen_asr(target_os: &str) {
    const VENDOR: &str = "vendor/qwen-asr";
    const SOURCES: &[&str] = &[
        "qwen_asr.c",
        "qwen_asr_kernels.c",
        "qwen_asr_kernels_generic.c",
        "qwen_asr_kernels_neon.c",
        "qwen_asr_kernels_avx.c",
        "qwen_asr_audio.c",
        "qwen_asr_encoder.c",
        "qwen_asr_decoder.c",
        "qwen_asr_tokenizer.c",
        "qwen_asr_safetensors.c",
    ];

    let mut build = cc::Build::new();
    build
        .include(VENDOR)
        .flag("-O3")
        .flag("-ffast-math")
        // 上游开 `-Wall -Wextra`；我们把 qwen-asr 的代码当三方依赖，把无关警告压成静默
        // 避免 build log 噪音淹没我们自己的告警。
        .flag("-Wno-unused-parameter")
        .flag("-Wno-unused-variable")
        .flag("-Wno-unused-function")
        .flag("-Wno-sign-compare")
        .warnings(false);

    if target_os == "macos" {
        build
            .define("USE_BLAS", None)
            .define("ACCELERATE_NEW_LAPACK", None);
    }

    for src in SOURCES {
        let path = format!("{}/{}", VENDOR, src);
        println!("cargo:rerun-if-changed={}", path);
        build.file(path);
    }
    println!("cargo:rerun-if-changed={}/qwen_asr.h", VENDOR);

    build.compile("qwen_asr");

    // BLAS = Accelerate
    if target_os == "macos" {
        println!("cargo:rustc-link-lib=framework=Accelerate");
    }

    // Linux 不依赖发行版的 OpenBLAS 开发包，先走 C 引擎自带的通用 CPU kernels。
    if target_os == "linux" {
        println!("cargo:rustc-link-lib=m");
        println!("cargo:rustc-link-lib=pthread");
    }

    // Apple Speech 本地 ASR（issue #574）：apple_speech_provider 用
    // SFSpeechRecognizer / SFSpeechURLRecognitionRequest，符号在 Speech.framework。
    if target_os == "macos" {
        println!("cargo:rustc-link-lib=framework=Speech");
    }
}
