use cloud_storage::{Client, Object};
use dotenvy::dotenv;
use futures::{StreamExt, future};
use ini::ini;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::io::AsyncBufReadExt;
use tokio::{fs, io};
use walkdir::WalkDir;
use zip::write::FileOptions;
use zip::{ZipArchive, ZipWriter};

struct IniConfig {
    target_dir: String,
    upload_dir: String,
    bucket_name: String,
    max_size: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting");
    dotenv().ok();

    verify_required_files();

    let ini_config = verify_config();

    match env::var("GOOGLE_APPLICATION_CREDENTIALS") {
        Ok(val) => println!("Credentials found at {}", val),
        Err(_) => panic!("No credentials found"),
    }

    println!("\n---- Enter a command ----");
    println!("u -- upload files from upload file to the server");
    println!("s -- sync files to target directory from the server");
    println!("l -- list current files and also write to a local .txt");
    println!("p -- compare local files to server files");
    println!("d -- delete a file");
    println!();

    let stdin = io::stdin();

    let mut reader = io::BufReader::new(stdin);
    let mut input: String = String::new();

    reader.read_line(&mut input).await?;

    if input.trim() == "u" {
        sync_to_server(&ini_config).await?;
    } else if input.trim() == "s" {
        sync_with_server(&ini_config).await?;
    } else if input.trim() == "l" {
        display_server_files().await?;
    } else if input.trim() == "p" {
        display_different_files(&ini_config).await?;
    } else if input.trim() == "d" {
        delete_file(&ini_config).await?;
    }

    println!("Enter any key to exit");
    reader.read_line(&mut input).await?;

    Ok(())
}

async fn delete_file(config: &IniConfig) -> Result<(), Box<dyn std::error::Error>> {
    let mut server_files = list_server_files()
        .await?
        .iter()
        .map(|file| file.clone())
        .collect::<Vec<String>>();
    server_files.sort();

    server_files
        .iter()
        .enumerate()
        .for_each(|(i, file)| println!("{}: {}", i, file));

    println!("Enter the index of the file to delete:");

    let stdin = io::stdin();

    let mut reader = io::BufReader::new(stdin);
    let mut input: String = String::new();

    reader.read_line(&mut input).await?;

    let input = input.trim().parse::<usize>()?;

    if input >= server_files.len() {
        panic!("Invalid index");
    }

    let file_name = server_files
        .get(input)
        .expect("Index out of bounds -- this shouldn't happen here");

    let client = Client::new();

    client
        .object()
        .delete(
            config.bucket_name.as_str(),
            format!("{}.zip", file_name).as_str(),
        )
        .await?;

    println!("Successfully deleted file: {}", file_name);

    Ok(())
}

async fn display_different_files(config: &IniConfig) -> Result<(), Box<dyn std::error::Error>> {
    let server_files = list_server_files().await?;
    let local_files = list_local_files(&config.upload_dir)?;

    let mut upload_files = local_files
        .difference(&server_files)
        .collect::<Vec<&String>>();

    upload_files.sort();

    for file in &upload_files {
        println!("{}", file);
    }

    let time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?;

    let file_name = format!("song-diff-{:?}.txt", time);

    // Write to a local .txt file
    let txt_path = Path::new(&file_name);

    let content = upload_files
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<&str>>()
        .join("\n");
    tokio::fs::write(txt_path, content).await?;

    Ok(())
}

async fn display_server_files() -> Result<(), Box<dyn std::error::Error>> {
    let mut server_files: Vec<String> = list_server_files()
        .await?
        .iter()
        .map(|file| file.clone())
        .collect();

    println!("Found {} file(s) on server", server_files.len());

    server_files.sort();

    for file in &server_files {
        println!("{}", file);
    }

    let time = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();

    let file_name = format!("song-list-{}.txt", time);

    // Write to a local .txt file
    let txt_path = Path::new(&file_name);

    let content = server_files.join("\n");
    tokio::fs::write(txt_path, content).await?;

    Ok(())
}

async fn sync_to_server(config: &IniConfig) -> Result<(), Box<dyn std::error::Error>> {
    println!("Checking for new files");

    let local_files = list_local_files(&config.upload_dir)?;
    let server_files = list_server_files().await?;

    println!("Found {} file(s) on server", server_files.len());

    let upload_files = local_files
        .difference(&server_files)
        .collect::<Vec<&String>>();

    println!(
        "Found {} file(s) to upload {:?}",
        upload_files.len(),
        upload_files
    );

    if upload_files.is_empty() {
        println!("No files to upload");
        return Ok(());
    }

    let client = Client::new();

    let futures: Vec<_> = upload_files
        .iter()
        .map(|file_name| zip_and_upload(&client, &config, *file_name))
        .collect();

    let results = future::join_all(futures).await;

    for result in results {
        if let Err(err) = result {
            eprintln!("Error uploading file: {}", err);
        }
    }

    Ok(())
}

async fn zip_and_upload(
    client: &Client,
    config: &IniConfig,
    file_name: &String,
) -> Result<(), Box<dyn std::error::Error>> {
    let upload_base = PathBuf::from(&config.upload_dir);
    let specific_song_path = upload_base.join(file_name);

    // Create the zip in the base upload directory, NOT inside specific_song_path
    let temp_zip_path = upload_base.join(format!("{}.zip", file_name));

    let temp_zip_path_clone = temp_zip_path.clone();

    let zip_result = tokio::task::spawn_blocking(move || {
        zip_directory_sync(&specific_song_path, &temp_zip_path)
            .map_err(|err| std::io::Error::other(err.to_string()))
    })
    .await;

    let result = match zip_result {
        Ok(Ok(())) => upload_file(client, config, &temp_zip_path_clone, file_name).await,
        Ok(Err(err)) => Err(err.into()),
        Err(err) => Err(err.into()),
    };

    // Clean up temporary archives even when packaging or uploading fails.
    if let Err(err) = fs::remove_file(&temp_zip_path_clone).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            eprintln!(
                "Failed to remove temporary archive {:?}: {}",
                temp_zip_path_clone, err
            );
        }
    }
    result?;

    Ok(())
}

fn zip_directory_sync(
    source_dir: &Path,
    target_zip_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::create(target_zip_path)?;

    let mut zip = ZipWriter::new(file);
    let options = FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);

    let mut buffer = Vec::new();

    for entry in WalkDir::new(source_dir) {
        let entry = entry?;
        let path = entry.path();
        let name = path.strip_prefix(source_dir)?;

        if path.is_file() {
            if is_visual_media(path) {
                println!("Skipping image/video: {}", path.display());
                continue;
            }
            zip.start_file(name.to_str().unwrap(), options)?;

            let mut f = File::open(path)?;
            f.read_to_end(&mut buffer)?;
            zip.write_all(&buffer)?;

            buffer.clear();
        }
    }

    zip.finish()?;
    println!("Successfully zipped directory");

    Ok(())
}

// Match extensions case-insensitively, including media in nested folders.
fn is_visual_media(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "bmp"
            | "webp"
            | "tif"
            | "tiff"
            | "tga"
            | "dds"
            | "avif"
            | "heic"
            | "svg"
            | "mp4"
            | "m4v"
            | "mkv"
            | "webm"
            | "avi"
            | "mov"
            | "wmv"
            | "mpg"
            | "mpeg"
            | "ogv"
            | "flv"
            | "3gp"
            | "vob"
            | "ts"
            | "m2ts"
    )
}

fn check_upload_size(size_bytes: u64, max_size_mib: u64) -> Result<(), std::io::Error> {
    let max_bytes = max_size_mib.checked_mul(1024 * 1024).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "max_size is too large")
    })?;
    if size_bytes > max_bytes {
        return Err(std::io::Error::other(format!(
            "Archive is {} bytes, exceeding max_size of {} MiB; skipping upload",
            size_bytes, max_size_mib
        )));
    }
    Ok(())
}

async fn upload_file(
    client: &Client,
    config: &IniConfig,
    zipped_file_path: &Path,
    zip_file_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    check_upload_size(fs::metadata(zipped_file_path).await?.len(), config.max_size)?;
    let content = fs::read(zipped_file_path).await?;
    // Verify the actual payload too, in case the file changed after the metadata check.
    check_upload_size(content.len() as u64, config.max_size)?;

    client
        .object()
        .create(
            &config.bucket_name,
            content,
            format!("{}.zip", zip_file_name).as_str(),
            "application/zip",
        )
        .await?;

    println!("Successfully uploaded file: {}", zip_file_name);

    Ok(())
}

async fn sync_with_server(config: &IniConfig) -> Result<(), Box<dyn std::error::Error>> {
    println!("Checking for new files");

    let local_files = list_local_files(&config.target_dir)?;
    let server_files = list_server_files().await?;

    let download_files: Vec<String> = server_files
        .iter()
        .filter(|file_name| !local_files.contains(*file_name))
        .map(|file_name| file_name.clone())
        .collect();

    println!(
        "\nFound {} file(s) that aren't synced",
        download_files.len()
    );

    println!("{:?}", download_files);

    let client = Client::new();

    let futures: Vec<_> = download_files
        .iter()
        .map(|file_name| download_file(&client, file_name, &config))
        .collect();

    let results: Vec<Result<(), Box<dyn std::error::Error>>> = future::join_all(futures).await;

    for result in results {
        if let Err(err) = result {
            eprintln!("Error downloading file: {}", err);
        }
    }

    Ok(())
}

async fn download_file(
    client: &Client,
    file_name: &String,
    config: &IniConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("Downloading: {}.zip", file_name);

    let gcs_object_name = format!("{}.zip", file_name);

    let downloaded_bytes: Vec<u8> = client
        .object()
        .download(&config.bucket_name, &gcs_object_name)
        .await?;

    println!(
        "Successfully downloaded {} bytes for '{}'.",
        downloaded_bytes.len(),
        gcs_object_name
    );

    let base_output_dir = PathBuf::from(&config.target_dir);
    let extraction_target_dir = base_output_dir.join(file_name);
    let temp_zip_path = base_output_dir.join(&gcs_object_name);

    fs::create_dir_all(&base_output_dir).await?;

    if extraction_target_dir.exists() {
        if extraction_target_dir.is_dir() {
            println!(
                "Clearing existing directory for extraction: {:?}",
                extraction_target_dir
            );
            fs::remove_dir_all(&extraction_target_dir).await?;
        } else {
            println!(
                "Removing conflicting file at extraction path: {:?}",
                extraction_target_dir
            );
            fs::remove_file(&extraction_target_dir).await?;
        }
    }
    fs::create_dir_all(&extraction_target_dir).await?;

    fs::write(&temp_zip_path, &downloaded_bytes).await?;
    println!(
        "Saved downloaded bytes to temporary zip file: '{:?}'",
        temp_zip_path
    );

    let cursor = Cursor::new(downloaded_bytes);
    let mut archive = ZipArchive::new(cursor)?;

    let mut common_parent_to_strip: Option<PathBuf> = None;
    let mut potential_paths_in_zip: Vec<PathBuf> = Vec::new();

    // First pass: collect all non-__MACOSX paths to determine if a common parent needs stripping
    for i in 0..archive.len() {
        // Removed explicit type annotation 'ZipFile'
        let file_in_zip = archive.by_index(i)?;
        let path_in_zip = PathBuf::from(file_in_zip.name());

        if path_in_zip.starts_with("__MACOSX") {
            continue;
        }
        potential_paths_in_zip.push(path_in_zip);
    }

    // Check if all collected paths share a single common top-level directory
    // that matches the `file_name` (e.g., "KISS - I Was Made for Lovin' You (Kaduzera)")
    if !potential_paths_in_zip.is_empty() {
        let expected_top_level_dir = PathBuf::from(file_name);
        let mut all_start_with_expected_dir = true;

        for p in &potential_paths_in_zip {
            // Check if the path starts with the expected top-level directory
            if !p.starts_with(&expected_top_level_dir) {
                all_start_with_expected_dir = false;
                break;
            }
            // Also ensure the expected_top_level_dir is actually a direct component, not just a prefix
            // (e.g., "song.ini" should not match "song" as a prefix, but "folder/song.ini" should match "folder")
            if let Some(first_comp) = p.components().next() {
                if first_comp.as_os_str() != expected_top_level_dir.as_os_str() {
                    all_start_with_expected_dir = false;
                    break;
                }
            } else {
                // Handle cases where path is empty or root (shouldn't happen for valid zip entries)
                all_start_with_expected_dir = false;
                break;
            }
        }

        if all_start_with_expected_dir {
            common_parent_to_strip = Some(expected_top_level_dir);
        }
    }

    // Second pass: extract files, skipping __MACOSX and stripping common parent if found
    for i in 0..archive.len() {
        // Removed explicit type annotation 'ZipFile'
        let mut file = archive.by_index(i)?;
        let path_in_zip = PathBuf::from(file.name());

        // Skip __MACOSX files
        if path_in_zip.starts_with("__MACOSX") {
            continue;
        }

        let mut relative_path = path_in_zip;

        // If a common top-level directory was identified, strip it
        if let Some(prefix_to_strip) = &common_parent_to_strip {
            if let Ok(stripped) = relative_path.strip_prefix(prefix_to_strip) {
                relative_path = stripped.to_path_buf();
            }
        }

        // If stripping results in an empty path (e.g., if the root directory entry itself was stripped),
        // or if the path is otherwise empty after stripping, just skip it.
        // This prevents trying to create extraction_target_dir inside itself.
        if relative_path.as_os_str().is_empty() {
            continue;
        }

        let outpath = extraction_target_dir.join(&relative_path);

        if file.is_dir() {
            fs::create_dir_all(&outpath).await?;
        } else {
            if let Some(p) = outpath.parent() {
                if !p.exists() {
                    fs::create_dir_all(&p).await?;
                }
            }
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)?;
            tokio::fs::write(&outpath, &buffer).await?;
        }
    }

    fs::remove_file(&temp_zip_path).await?;

    Ok(())
}

fn list_local_files(dir: &String) -> Result<HashSet<String>, Box<dyn std::error::Error>> {
    let mut local_files: HashSet<String> = HashSet::new();
    let path = Path::new(dir);

    for entry in path.read_dir()? {
        match entry {
            Ok(res) => match res.file_type() {
                Ok(file_type) => {
                    if file_type.is_dir() {
                        local_files.insert(
                            res.file_name()
                                .into_string()
                                .expect("Error converting to string"),
                        );
                    }
                }
                Err(err) => {
                    println!("Error reading file: {} {}", err, res.path().display());
                }
            },
            Err(err) => {
                panic!("Error reading file: {}", err);
            }
        }
    }

    Ok(local_files)
}

async fn list_server_files() -> Result<HashSet<String>, Box<dyn std::error::Error>> {
    let mut server_files: HashSet<String> = HashSet::new();
    let list_request = cloud_storage::ListRequest::default();

    let bucket_name = "c-hero";

    let objects = Object::list(bucket_name, list_request).await?;

    tokio::pin!(objects);

    while let Some(object) = objects.next().await {
        match object {
            Ok(res) => {
                for object in res.items {
                    let file_name = object.name.clone();

                    let zipped_file_name = check_zipped_file(file_name.clone());

                    if zipped_file_name.is_some() {
                        server_files.insert(zipped_file_name.unwrap());
                    }
                }
            }
            Err(err) => {
                panic!("Error reading file: {}", err);
            }
        }
    }

    Ok(server_files)
}

fn check_zipped_file(file_name: String) -> Option<String> {
    let path = PathBuf::from(&file_name);

    if let Some(ext) = path.extension() {
        if ext.to_ascii_lowercase() == "zip" {
            return Some(path.file_stem()?.to_str()?.to_string());
        }
    }

    None
}

fn verify_required_files() {
    if File::open("gcp-key.json").is_err() {
        panic!("gcp-key.json not found add it and try again");
    }

    if File::open("config.ini").is_err() {
        panic!(".env file not found add it and try again");
    }

    if File::open(".env").is_err() {
        panic!(".env file not found add it and try again");
    }
}

fn verify_config() -> IniConfig {
    let config_map = ini!("config.ini");

    let local_section_keys: (&str, Vec<&str>) =
        ("local", vec!["target_dir", "upload_dir", "max_size"]);
    let google_section_keys = ("googlecloud", vec!["bucket_name"]);

    let local_keys = check_keys(local_section_keys.0, local_section_keys.1, &config_map);
    let google_keys = check_keys(google_section_keys.0, google_section_keys.1, &config_map);

    IniConfig {
        upload_dir: local_keys.get("upload_dir").unwrap().to_string(),
        target_dir: local_keys.get("target_dir").unwrap().to_string(),
        bucket_name: google_keys.get("bucket_name").unwrap().to_string(),
        max_size: local_keys
            .get("max_size")
            .unwrap_or(&"10".to_string())
            .parse::<u64>()
            .unwrap(),
    }
}

fn check_keys(
    target: &str,
    keys: Vec<&str>,
    config: &HashMap<String, HashMap<String, Option<String>>>,
) -> HashMap<String, String> {
    let mut ret_map = HashMap::new();

    if !config.contains_key(target) {
        panic!(
            "{} section not found in config.ini (it is either empty or doesn't exist)",
            target
        );
    }

    let map = config.get(target).unwrap();

    keys.iter().for_each(|key| {
        let found_key = map.get_key_value(*key);

        if found_key.is_none() {
            panic!("{} is required in config.ini", key);
        }

        ret_map.insert(key.to_string(), found_key.unwrap().1.clone().unwrap());
    });

    ret_map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_media_detection_preserves_song_files() {
        for name in [
            "album.JPG",
            "nested/background.png",
            "video.MP4",
            "video.webm",
        ] {
            assert!(is_visual_media(Path::new(name)), "{name}");
        }
        for name in [
            "song.ogg",
            "guitar.opus",
            "song.mp3",
            "notes.chart",
            "notes.mid",
            "song.ini",
            "README",
        ] {
            assert!(!is_visual_media(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn upload_limit_checks_exact_bytes_and_overflow() {
        assert!(check_upload_size(1024 * 1024, 1).is_ok());
        assert!(check_upload_size(1024 * 1024 + 1, 1).is_err());
        assert!(check_upload_size(1, 0).is_err());
        assert!(check_upload_size(1, u64::MAX).is_err());
    }

    #[tokio::test]
    async fn oversized_song_is_discovered_and_packaged_without_visual_media() {
        let temp = env::temp_dir().join(format!(
            "file-sync-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let song = temp.join("Song");
        std::fs::create_dir_all(song.join("nested")).unwrap();
        for (name, content) in [
            ("song.ini", "[song]"),
            ("notes.chart", "chart"),
            ("song.ogg", "audio"),
        ] {
            std::fs::write(song.join(name), content).unwrap();
        }
        std::fs::write(song.join("nested/video.MP4"), vec![1; 2 * 1024 * 1024]).unwrap();
        std::fs::write(song.join("album.JPG"), b"image").unwrap();
        assert!(
            list_local_files(&temp.to_str().unwrap().to_string())
                .unwrap()
                .contains("Song")
        );

        let archive_path = temp.join("Song.zip");
        zip_directory_sync(&song, &archive_path).unwrap();
        let mut archive = ZipArchive::new(File::open(&archive_path).unwrap()).unwrap();
        assert_eq!(archive.len(), 3);
        for (name, expected) in [
            ("song.ini", "[song]"),
            ("notes.chart", "chart"),
            ("song.ogg", "audio"),
        ] {
            let mut content = String::new();
            archive
                .by_name(name)
                .unwrap()
                .read_to_string(&mut content)
                .unwrap();
            assert_eq!(content, expected);
        }
        assert!(song.join("nested/video.MP4").exists());
        assert!(check_upload_size(std::fs::metadata(&archive_path).unwrap().len(), 1).is_ok());
        drop(archive);

        // A zero limit must reject the ZIP before any cloud request is made.
        let config = IniConfig {
            target_dir: String::new(),
            upload_dir: temp.to_str().unwrap().to_string(),
            bucket_name: String::new(),
            max_size: 0,
        };
        let err = zip_and_upload(&Client::new(), &config, &"Song".to_string())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exceeding max_size"));
        assert!(!archive_path.exists());
        assert!(song.join("song.ogg").exists());
        std::fs::remove_dir_all(temp).unwrap();
    }
}
