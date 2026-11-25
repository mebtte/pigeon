mod archive;
mod client;
mod options;

use std::{
    io::{self, Write},
    path::PathBuf,
};

use clap::Args;

use crate::config::parse_credentials;

use self::{
    archive::Selection,
    client::{Client, link_with_password},
    options::ShareOptions,
};

#[derive(Args)]
pub(crate) struct ShareArguments {
    #[arg(long, value_name = "URL", help = "Pigeon deployment URL")]
    endpoint: String,

    #[arg(long, value_name = "USERNAME:PASSWORD", help = "Pigeon login account")]
    user: String,

    #[arg(required = true, num_args = 1.., value_name = "INPUT", help = "Files or directories to share")]
    paths: Vec<PathBuf>,
}

pub(crate) async fn run(args: ShareArguments) -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new(&args.endpoint)?;
    let (username, password) = parse_credentials(&args.user).map_err(io::Error::other)?;
    let selection = archive::inspect(args.paths.clone())?;
    let options = {
        let mut output = io::stderr().lock();
        writeln!(output, "Endpoint: {}", args.endpoint)?;
        writeln!(output, "Share as {:?}:", selection.filename())?;
        for path in &args.paths {
            writeln!(output, "  {path:?}")?;
        }
        options::prompt(&mut io::stdin().lock(), &mut output)?
    };

    let Some(options) = options else {
        eprintln!("Share canceled.");
        return Ok(());
    };

    let token = client.login(username, password).await?;
    let result = upload(&client, &token, selection, &options).await;
    // CLI sessions are temporary, including when preparation or upload fails.
    let _ = client.logout(&token).await;

    let link = result?;
    let mut output = io::stdout().lock();
    writeln!(output, "{link}")?;
    if let Some(password) = options.password.as_deref() {
        writeln!(output, "{}", link_with_password(&link, password))?;
    }
    Ok(())
}

async fn upload(
    client: &Client,
    token: &str,
    selection: Selection,
    options: &ShareOptions,
) -> Result<reqwest::Url, Box<dyn std::error::Error>> {
    let limit = client.max_file_size(token).await?;
    let filename = selection.filename().to_owned();
    eprintln!("Preparing {filename:?}...");
    let (file, size) = tokio::task::spawn_blocking(move || selection.prepare()).await??;
    if let Some(limit) = limit
        && size > limit
    {
        return Err(io::Error::other(format!(
            "{filename:?} is {size} bytes and exceeds the server's {limit}-byte upload limit"
        ))
        .into());
    }
    eprintln!("Uploading {filename:?} ({size} bytes)...");
    client.upload(token, file, &filename, size, options).await
}
