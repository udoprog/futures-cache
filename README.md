# futures-cache

[<img alt="github" src="https://img.shields.io/badge/github-udoprog/futures--cache-8da0cb?style=for-the-badge&logo=github" height="20">](https://github.com/udoprog/futures-cache)
[<img alt="crates.io" src="https://img.shields.io/crates/v/futures-cache.svg?style=for-the-badge&color=fc8d62&logo=rust" height="20">](https://crates.io/crates/futures-cache)
[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-futures--cache-66c2a5?style=for-the-badge&logoColor=white&logo=data:image/svg+xml;base64,PHN2ZyByb2xlPSJpbWciIHhtbG5zPSJodHRwOi8vd3d3LnczLm9yZy8yMDAwL3N2ZyIgdmlld0JveD0iMCAwIDUxMiA1MTIiPjxwYXRoIGZpbGw9IiNmNWY1ZjUiIGQ9Ik00ODguNiAyNTAuMkwzOTIgMjE0VjEwNS41YzAtMTUtOS4zLTI4LjQtMjMuNC0zMy43bC0xMDAtMzcuNWMtOC4xLTMuMS0xNy4xLTMuMS0yNS4zIDBsLTEwMCAzNy41Yy0xNC4xIDUuMy0yMy40IDE4LjctMjMuNCAzMy43VjIxNGwtOTYuNiAzNi4yQzkuMyAyNTUuNSAwIDI2OC45IDAgMjgzLjlWMzk0YzAgMTMuNiA3LjcgMjYuMSAxOS45IDMyLjJsMTAwIDUwYzEwLjEgNS4xIDIyLjEgNS4xIDMyLjIgMGwxMDMuOS01MiAxMDMuOSA1MmMxMC4xIDUuMSAyMi4xIDUuMSAzMi4yIDBsMTAwLTUwYzEyLjItNi4xIDE5LjktMTguNiAxOS45LTMyLjJWMjgzLjljMC0xNS05LjMtMjguNC0yMy40LTMzLjd6TTM1OCAyMTQuOGwtODUgMzEuOXYtNjguMmw4NS0zN3Y3My4zek0xNTQgMTA0LjFsMTAyLTM4LjIgMTAyIDM4LjJ2LjZsLTEwMiA0MS40LTEwMi00MS40di0uNnptODQgMjkxLjFsLTg1IDQyLjV2LTc5LjFsODUtMzguOHY3NS40em0wLTExMmwtMTAyIDQxLjQtMTAyLTQxLjR2LS42bDEwMi0zOC4yIDEwMiAzOC4ydi42em0yNDAgMTEybC04NSA0Mi41di03OS4xbDg1LTM4Ljh2NzUuNHptMC0xMTJsLTEwMiA0MS40LTEwMi00MS40di0uNmwxMDItMzguMiAxMDIgMzguMnYuNnoiPjwvcGF0aD48L3N2Zz4K" height="20">](https://docs.rs/futures-cache)
[<img alt="build status" src="https://img.shields.io/github/actions/workflow/status/udoprog/futures-cache/ci.yml?branch=main&style=for-the-badge" height="20">](https://github.com/udoprog/futures-cache/actions?query=branch%3Amain)

Futures-aware cache abstraction.

Provides a cache for asynchronous operations that persist data on the
filesystem using [sled]. The async cache works by accepting a future, but
will cancel the accepted future in case the answer is already in the cache.

It requires unique cache keys that are [serde] serializable. To distinguish
across different sub-components of the cache, they can be namespaces using
[Cache::namespaced].

[sled]: https://github.com/spacejam/sled

<br>

## State

The state of the library is:
* API is limited to only `wrap`, which includes a timeout ([#1]).
* Requests are currently racing in the `wrap` method, so multiple unecessary
  requests might occur when they should //! instead be queueing up ([#2]).
* Entries only expire when the library is loaded ([#3]).
* Only storage backend is sled ([#4]).

[#1]: https://github.com/udoprog/futures-cache/issues/1
[#2]: https://github.com/udoprog/futures-cache/issues/2
[#3]: https://github.com/udoprog/futures-cache/issues/3
[#4]: https://github.com/udoprog/futures-cache/issues/4

<br>

## Usage

This library requires the user to add the following dependencies to use:

```toml
futures-cache = "0.10.1"
serde = { version = "1.0", features = ["derive"] }
```

<br>

## Examples

Simple example showcasing fetching information on a github repository.

> This is also available as an example you can run with:
> ```sh
> cargo run --example github -- --user udoprog --repo futures-cache
> ```

```rust
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
        .header(header::USER_AGENT, "Reqwest/0.10")
        .build()?;

    let body = client.execute(req).await?.text().await?;
    Ok(body)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let db = sled::open("cache")?;
    let cache = Cache::load(db.open_tree("cache")?)?;

    let user = "udoprog";
    let repo = "futures-cache";

    let text = cache
        .wrap(
            GithubKey::Repo {
                user: user,
                repo: repo,
            },
            Duration::seconds(60),
            github_repo(user, repo),
        )
        .await?;

    println!("{}", text);
    Ok(())
}
```

[serde]: https://docs.rs/serde
[Cache::namespaced]: https://docs.rs/futures-cache/0/futures_cache/struct.Cache.html#method.namespaced
