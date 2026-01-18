use cloud_storage::{Client, Object};
use dotenvy::dotenv;
use futures::{StreamExt, future};
use ini::ini;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use tokio::io::AsyncBufReadExt;
use tokio::{fs, io};
use walkdir::WalkDir;
use zip::write::FileOptions;
use zip::{ZipArchive, ZipWriter};

struct IniConfig {
    target_dir: String,
    upload_dir: String,
    bucket_name: String,
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

    println!("Enter either u or s to continue");

    let stdin = io::stdin();

    let mut reader = io::BufReader::new(stdin);
    let mut input: String = String::new();

    reader.read_line(&mut input).await?;

    if input.trim() == "u" {
        sync_to_server(&ini_config).await?;
    } else if input.trim() == "s" {
        sync_with_server(&ini_config).await?;
    }

    Ok(())
}

async fn sync_to_server(config: &IniConfig) -> Result<(), Box<dyn std::error::Error>> {
    let local_files = list_local_files(&config.upload_dir)?;
    let server_files = list_server_files().await?;

    let upload_files = local_files
        .difference(&server_files)
        .collect::<Vec<&String>>();

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
    let source_path = PathBuf::from(&config.upload_dir);
    let temp_zip_path = source_path.join(format!("{}.zip", file_name));

    let temp_zip_path_clone = temp_zip_path.clone();

    tokio::task::spawn_blocking(move || {
        zip_directory_sync(&source_path, &temp_zip_path.clone()).expect("");
    })
    .await?;

    upload_file(client, &config, &temp_zip_path_clone, file_name).await?;
    // fs::remove_file(&temp_zip_path_clone).await?;

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
            zip.start_file(name.to_str().unwrap(), options)?;

            let mut f = File::open(path)?;
            f.read_to_end(&mut buffer)?;
            zip.write_all(&buffer)?;

            buffer.clear();
        }
    }

    zip.finish().expect("Failed to finish zip archive");
    println!("Successfully zipped directory");

    Ok(())
}

async fn upload_file(
    client: &Client,
    config: &IniConfig,
    zipped_file_path: &Path,
    zip_file_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let content = fs::read(zipped_file_path).await?;

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

                    if check_zipped_file(file_name.clone()) {
                        server_files
                            .insert(file_name.split(".").collect::<Vec<&str>>()[0].to_string());
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

fn check_zipped_file(file_name: String) -> bool {
    let name = file_name.split(".").collect::<Vec<&str>>();

    if name.len() < 2 || name.len() > 2 || name[1] != "zip" {
        return false;
    }

    true
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

    let local_section_keys: (&str, Vec<&str>) = ("local", vec!["target_dir", "upload_dir"]);
    let google_section_keys = ("googlecloud", vec!["bucket_name"]);

    let local_keys = check_keys(local_section_keys.0, local_section_keys.1, &config_map);
    let google_keys = check_keys(google_section_keys.0, google_section_keys.1, &config_map);


    IniConfig {
        upload_dir: local_keys.get("upload_dir").unwrap().to_string(),
        target_dir: local_keys.get("target_dir").unwrap().to_string(),
        bucket_name: google_keys.get("bucket_name").unwrap().to_string(),
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
