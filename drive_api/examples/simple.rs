use drive_api::{DriveApi, FilesReq, GoogleCredential, MoreFileFields};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // I recommend to put these values in a dotenv file, and then running this as
    // dotenv run cargo run --example simple

    let client_config = drive_api::oauth::ClientConfig {
        client_id: std::env::var("GOOGLE_CID").unwrap(),
        client_secret: std::env::var("GOOGLE_SECRET").unwrap()
    };
    let refresh_token = std::env::var("GOOGLE_RT").unwrap();

    let credential = GoogleCredential::new(client_config, refresh_token, None, |_| {
        println!("Recevied new token");
    });

    let api_client = DriveApi::new(Default::default(), credential);
    let root_file = api_client
        .files_get("root")
        .await?;


    for file in files_in_root(&api_client, &root_file.id).await? {
        println!("{:?}", file.name);
    }

    // Now create a file
    let test_file_id = create_test_file(&api_client, root_file.id.clone()).await;
    let files = files_in_root(&api_client, &root_file.id).await?;
    let retrieved = files.iter().find(|file| file.id == test_file_id).unwrap();
    match retrieved.more {
        Some(MoreFileFields::BinaryFile { size, .. }) => assert_eq!(size, 5),
        _ => panic!("unexpected")
    }

    // Delete the file and make sure it is gone
    api_client.files_delete(&test_file_id).await?;
    println!("Deleted the file");

    assert!(!files_in_root(&api_client, &root_file.id).await?.iter().any(|file| file.id == test_file_id));

    Ok(())
}

async fn files_in_root(api_client: &DriveApi, root_id: &str) -> anyhow::Result<Vec<drive_api::File>> {
    Ok(api_client.file_list_all(&format!("'{}' in parents", root_id)).await?)
}

async fn create_test_file(api_client: &DriveApi, folder_id: String) -> String {
    let value = api_client
        .files_create_upload(FilesReq {
            name: Some("test_file".into()),
            parents: Some(vec![folder_id]),
            ..Default::default()
        }, &b"Hello"[..], 5
        )
        .await
        .unwrap();

    println!("Created file {:?}", value);

    value.id
}