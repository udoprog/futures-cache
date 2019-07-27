# futures-cache

[![Build Status](https://travis-ci.org/udoprog/futures-cache.svg?branch=master)](https://travis-ci.org/udoprog/futures-cache)

Futures-aware cache backed by sled.

## State

The state of the library is:
* API is limited to only `wrap`, which includes a timeout ([#1]).
* Requests are currently racing in the `wrap` method, so multiple unecessary requests might occur when they should instead be queueing up ([#2]).
* Entries only expire when the library is loaded ([#3]).
* Only storage backend is sled ([#4]).

[#1]: https://github.com/udoprog/futures-cache/issues/1
[#2]: https://github.com/udoprog/futures-cache/issues/2
[#3]: https://github.com/udoprog/futures-cache/issues/3
[#4]: https://github.com/udoprog/futures-cache/issues/4

## Usage

This library requires the user to add the following dependencies to use:

```toml
futures-cache = "0.2.0"
serde = {version = "1.0.97", features = ["derive"]}
```

## Examples

```rust
use std::{path::Path, fs, error::Error, sync::Arc};
use futures_cache::{sled, Duration, Cache};
use futures;
use serde::{Serialize, Deserialize};

#[derive(Debug, Serialize)]
enum Key<'a> {
    /// Cache key for the get_user request.
    GetUser(&'a str),
}

fn setup_db() -> Result<sled::Db, Box<dyn Error>> {
    let path = Path::new("path/to/cache");

    if !path.is_dir() {
        fs::create_dir_all(path)?;
    }

    Ok(sled::Db::start_default(path)?)
}

fn main() -> Result<(), Box<dyn Error>> {
    let db = setup_db()?;
    let cache = Cache::load(db.open_tree("cache")?)?;
    let api = Api::new()?;

    futures::executor::block_on(async move {
        // Cache for 12 hours.
        let mary = cache.wrap(Key::GetUser("mary"), Duration::hours(12), api.get_user("mary")).await?;
        // Second request will be cached.
        let mary2 = cache.wrap(Key::GetUser("mary"), Duration::hours(12), api.get_user("mary")).await?;
    });
}
```