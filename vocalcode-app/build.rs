fn main() {
    // Re-embed when the icon changes (Cargo won't re-run this otherwise).
    println!("cargo:rerun-if-changed=vocalcode.ico");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=VOCALCODE_LICENSE_PUBLIC_KEY_B64");

    // A release without this key accepts no signed receipt, which would turn
    // every paying customer back into a trial user.  Fail the build instead of
    // producing a perfectly signed but commercially unusable installer.
    let community = std::env::var_os("CARGO_FEATURE_COMMUNITY").is_some();
    if std::env::var("PROFILE").as_deref() == Ok("release") && !community {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        let encoded = std::env::var("VOCALCODE_LICENSE_PUBLIC_KEY_B64")
            .expect("release requires VOCALCODE_LICENSE_PUBLIC_KEY_B64");
        let encoded = encoded.trim();
        assert_ne!(
            encoded, "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=",
            "the public receipt-contract fixture key must never be used in a release"
        );
        let decoded = STANDARD
            .decode(encoded)
            .expect("VOCALCODE_LICENSE_PUBLIC_KEY_B64 must be valid base64");
        assert_eq!(
            decoded.len(),
            32,
            "VOCALCODE_LICENSE_PUBLIC_KEY_B64 must contain one 32-byte Ed25519 public key"
        );
        assert_eq!(
            STANDARD.encode(&decoded),
            encoded,
            "VOCALCODE_LICENSE_PUBLIC_KEY_B64 must use canonical base64"
        );
        assert!(
            decoded.iter().any(|byte| *byte != 0),
            "VOCALCODE_LICENSE_PUBLIC_KEY_B64 must not be the all-zero key"
        );
    }
    // Embed the app icon into the Windows exe (Explorer, taskbar, installer).
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("vocalcode.ico");
        // One product in both builds: the same names the paid releases'
        // VocalCode.exe carried, since the free build now installs over it.
        res.set("FileDescription", "VocalCode");
        res.set("ProductName", "VocalCode");
        res.set("InternalName", "VocalCode");
        res.set("OriginalFilename", "VocalCode.exe");
        if let Err(error) = res.compile() {
            if std::env::var("PROFILE").as_deref() == Ok("release") {
                panic!("release requires a working Windows resource toolchain: {error}");
            }
            println!("cargo:warning=Windows icon resource was not embedded: {error}");
        }
    }
}
