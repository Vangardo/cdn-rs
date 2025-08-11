use redis::{aio::ConnectionManager, AsyncCommands, Client};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct Cache {
    conn: Option<Arc<Mutex<ConnectionManager>>>,
}

impl Cache {
    pub async fn new(redis_url: Option<&str>) -> redis::RedisResult<Self> {
        if let Some(url) = redis_url {
            let client = Client::open(url)?;
            let manager = ConnectionManager::new(client).await?;
            Ok(Self {
                conn: Some(Arc::new(Mutex::new(manager))),
            })
        } else {
            Ok(Self { conn: None })
        }
    }

    pub async fn get(&self, key: &str) -> redis::RedisResult<Option<Vec<u8>>> {
        if let Some(conn) = &self.conn {
            let mut conn = conn.lock().await;
            conn.get(key).await
        } else {
            Ok(None)
        }
    }

    pub async fn insert_with_ttl(
        &self,
        key: &str,
        value: &[u8],
        ttl_secs: usize,
        max: usize,
    ) -> redis::RedisResult<()> {
        if let Some(conn) = &self.conn {
            let mut conn = conn.lock().await;
            let list_key = "image_cache_keys";
            conn.set_ex::<_, _, ()>(key, value, ttl_secs as u64).await?;
            conn.lpush::<_, _, ()>(list_key, key).await?;
            let len: usize = conn.llen(list_key).await?;
            if len > max {
                let to_remove: Vec<String> = conn.lrange(list_key, max as isize, -1).await?;
                for old in to_remove {
                    let _: () = conn.del(old).await?;
                }
                let _: () = conn.ltrim(list_key, 0, (max as isize) - 1).await?;
            }
        }
        Ok(())
    }
}
