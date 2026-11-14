use std::env;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/logo.png");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let logo_path = manifest_dir.join("assets/logo.png");
    let ico_path = manifest_dir.join("assets/marina.ico");

    // Generate ICO file from PNG using the image crate
    if let Err(e) = generate_ico(&logo_path, &ico_path) {
        println!("cargo:warning=Failed to generate ICO: {e}");
        return;
    }

    // Now compile the RC file
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let res = out_dir.join("marina.res");
    let marina_rc = manifest_dir.join("marina.rc");

    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe not found; the exe will have no icon");
        return;
    };

    let status = Command::new(&rc)
        .current_dir(&manifest_dir)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res)
        .arg(&marina_rc)
        .status();

    match status {
        Ok(status) if status.success() => {
            println!("cargo:rustc-link-arg-bins={}", res.display());
        }
        _ => println!("cargo:warning=rc.exe failed; the exe will have no icon"),
    }
}

fn generate_ico(png_path: &Path, ico_path: &Path) -> Result<(), String> {
    use image::imageops::FilterType;

    // Load the source PNG
    let img = image::open(png_path).map_err(|e| format!("Failed to load logo.png: {e}"))?;

    // ICO file format: https://en.wikipedia.org/wiki/ICO_(file_format)
    let mut ico_data = Vec::new();

    // ICO Header (6 bytes)
    ico_data.extend_from_slice(&[0, 0]); // Reserved (must be 0)
    ico_data.extend_from_slice(&[1, 0]); // Type: 1 = ICO

    let sizes = [16u32, 32, 48, 64]; // Common Windows icon sizes
    ico_data.extend_from_slice(&(sizes.len() as u16).to_le_bytes()); // Number of images

    // Calculate offsets for each image
    let header_size = 6;
    let dir_entry_size = 16;
    let dir_size = header_size + dir_entry_size * sizes.len();

    let mut image_data = Vec::new();
    let mut offsets = Vec::new();

    // Generate resized PNGs for each size
    for &size in &sizes {
        let resized = img.resize_exact(size, size, FilterType::Lanczos3);
        let mut png_bytes = Vec::new();
        resized
            .write_to(
                &mut std::io::Cursor::new(&mut png_bytes),
                image::ImageFormat::Png,
            )
            .map_err(|e| format!("Failed to encode PNG: {e}"))?;

        offsets.push((dir_size + image_data.len()) as u32);
        image_data.extend_from_slice(&png_bytes);
    }

    // Write directory entries
    for (i, &size) in sizes.iter().enumerate() {
        let width = if size < 256 { size as u8 } else { 0 };
        let height = if size < 256 { size as u8 } else { 0 };

        ico_data.push(width); // Width (0 = 256)
        ico_data.push(height); // Height (0 = 256)
        ico_data.push(0); // Color palette (0 = no palette)
        ico_data.push(0); // Reserved
        ico_data.extend_from_slice(&[1, 0]); // Color planes
        ico_data.extend_from_slice(&[32, 0]); // Bits per pixel (32 = RGBA)

        let start = offsets[i];
        let end = if i + 1 < offsets.len() {
            offsets[i + 1]
        } else {
            (dir_size + image_data.len()) as u32
        };
        let size_bytes = end - start;

        ico_data.extend_from_slice(&size_bytes.to_le_bytes()); // Image size
        ico_data.extend_from_slice(&start.to_le_bytes()); // Image offset
    }

    // Append all image data
    ico_data.extend_from_slice(&image_data);

    // Write ICO file
    let mut file = File::create(ico_path).map_err(|e| format!("Failed to create ICO file: {e}"))?;
    file.write_all(&ico_data)
        .map_err(|e| format!("Failed to write ICO file: {e}"))?;

    Ok(())
}

fn find_rc() -> Option<PathBuf> {
    if let Ok(path) = env::var("RC") {
        return Some(PathBuf::from(path));
    }
    if Command::new("rc.exe").arg("/?").output().is_ok() {
        return Some(PathBuf::from("rc.exe"));
    }
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86") => "x86",
        _ => "x64",
    };
    let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(kits)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.join(arch).join("rc.exe").is_file())
        .collect();
    versions.sort();
    versions.pop().map(|path| path.join(arch).join("rc.exe"))
}
