# futures-cache

Futures-aware cache abstraction

Provides a cache for asynchronous operations that persist data on the
filesystem using [sled]. The async cache works by accepting a future, but
will cancel the accepted future in case the answer is already in the cache.

It requires unique cache keys that are [serde] serializable. To distinguish
across different sub-components of the cache, they can be namespaces using
[Cache::namespaced].

[sled]: https://github.com/spacejam/sled

### State

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

### Usage

This library requires the user to add the following dependencies to use:

```toml
futures-cache = "0.3.0"
serde = {version = "1.0.97", features = ["derive"]}
```

### Examples

Simple example showcasing fetching information on a github repository.

> This is also available as an example you can run with:
> ```
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
        .header(header::USER_AGENT, "Reqwest/0.10.10")
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

License: MIT/Apache-2.0
