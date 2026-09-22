use std::{env, fs, io, path::PathBuf};

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let icon_path = out_dir.join("comfyui-mcp.ico");
    fs::write(&icon_path, make_icon())?;

    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(icon_path.to_string_lossy().as_ref())
        .set("FileDescription", "ComfyUI MCP Bridge")
        .set("ProductName", "ComfyUI MCP Bridge")
        .set("LegalCopyright", "MIT License")
        .set_manifest(
            r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
<assemblyIdentity version="1.0.0.0" processorArchitecture="*" name="ComfyUI.MCP.Bridge" type="win32"/>
<description>ComfyUI MCP Bridge</description>
<dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/></dependentAssembly></dependency>
<trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges><requestedExecutionLevel level="asInvoker" uiAccess="false"/></requestedPrivileges></security></trustInfo>
<application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings><dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware></windowsSettings></application>
</assembly>"#,
        );
    resource.compile().map_err(io::Error::other)
}

fn make_icon() -> Vec<u8> {
    const W: usize = 32;
    const H: usize = 32;
    const XOR_BYTES: usize = W * H * 4;
    const MASK_BYTES: usize = W * 4;
    const IMAGE_BYTES: usize = 40 + XOR_BYTES + MASK_BYTES;
    let mut output = Vec::with_capacity(22 + IMAGE_BYTES);

    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(&1_u16.to_le_bytes());
    output.extend_from_slice(&1_u16.to_le_bytes());
    output.extend_from_slice(&[W as u8, H as u8, 0, 0]);
    output.extend_from_slice(&1_u16.to_le_bytes());
    output.extend_from_slice(&32_u16.to_le_bytes());
    output.extend_from_slice(&(IMAGE_BYTES as u32).to_le_bytes());
    output.extend_from_slice(&22_u32.to_le_bytes());

    output.extend_from_slice(&40_u32.to_le_bytes());
    output.extend_from_slice(&(W as i32).to_le_bytes());
    output.extend_from_slice(&((H * 2) as i32).to_le_bytes());
    output.extend_from_slice(&1_u16.to_le_bytes());
    output.extend_from_slice(&32_u16.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    output.extend_from_slice(&((XOR_BYTES + MASK_BYTES) as u32).to_le_bytes());
    output.extend_from_slice(&0_i32.to_le_bytes());
    output.extend_from_slice(&0_i32.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());
    output.extend_from_slice(&0_u32.to_le_bytes());

    for y in (0..H).rev() {
        for x in 0..W {
            let dx = x.min(W - 1 - x);
            let dy = y.min(H - 1 - y);
            let inside = dx >= 3 || dy >= 3 || (dx + dy >= 4);
            let border = inside && (dx <= 3 || dy <= 3);
            let node = [(10_i32, 16_i32), (21, 10), (21, 22)]
                .iter()
                .any(|(nx, ny)| (x as i32 - nx).pow(2) + (y as i32 - ny).pow(2) <= 5);
            let edge = ((y as i32 - 16).abs() <= 1 && (10..=21).contains(&x))
                || ((x as i32 - 16).abs() <= 1 && (10..=22).contains(&y));
            let (r, g, b, a) = if node {
                (245, 250, 255, 255)
            } else if edge {
                (118, 227, 255, 255)
            } else if border {
                (71, 215, 255, 255)
            } else if inside {
                (35 + (x * 2) as u8, 28 + y as u8, 80 + (x * 3) as u8, 255)
            } else {
                (0, 0, 0, 0)
            };
            output.extend_from_slice(&[b, g, r, a]);
        }
    }
    output.resize(22 + IMAGE_BYTES, 0);
    output
}
