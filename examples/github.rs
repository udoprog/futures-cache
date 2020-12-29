use futures_cache::{Cache, Duration};
use serde::Serialize;

type Error = Box<dyn std::error::Error>;

#[derive(Debug, Serialize)]
enum GithubKey<'a> {
    Repo { user: &'a str, repo: &'a str },
}

async fn github_repo(user: &str, repo: &str) -> Result<String, Error> {
    use reqwest::header;
    use reqwest::{Client, Url};

    let client = Client::new();

    let url = Url::parse(&format!("https://api.github.com/repos/{}/{}", user, repo))?;

    let req = client
        .get(url)
        .header(header::USER_AGENT, "Reqwest/0.10.10")
        .build()?;

    let body = client.execute(req).await?.text().await?;
    Ok(body)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let db = sled::open("cache")?;
    let cache = Cache::load(db.open_tree("cache")?)?;

    let mut user = String::from("udoprog");
    let mut repo = String::from("futures-cache");

    let mut it = std::env::args();
    it.next();

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--user" => {
                user = it.next().ok_or_else(|| "missing argument to `--user`")?;
            }
            "--repo" => {
                repo = it.next().ok_or_else(|| "missing argument to `--repo`")?;
            }
            "-h" | "--help" => {
                println!("github [--user <user>] [--repo <repo>]");
                println!();
                println!("Fetches API result of the github api repo request for the given user and repo.");
                println!("Default to `udoprog/futures-cache`.");
                return Ok(());
            }
            other => {
                return Err(format!("unsupported argument `{}`", other).into());
            }
        }
    }

    let text = cache
        .wrap(
            GithubKey::Repo {
                user: &user,
                repo: &repo,
            },
            Duration::seconds(60),
            github_repo(&user, &repo),
        )
        .await?;

    println!("{}", text);
    Ok(())
}
