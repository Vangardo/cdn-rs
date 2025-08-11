use std::sync::Arc;
use redis::{aio::ConnectionManager, AsyncCommands, Client};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct Cache {
    conn: Arc<Mutex<ConnectionManager>>,
}

impl Cache {
    pub async fn new(redis_url: &str) -> redis::RedisResult<Self> {
        let client = Client::open(redis_url)?;
        let manager = ConnectionManager::new(client).await?;
        Ok(Self { conn: Arc::new(Mutex::new(manager)) })
    }

    pub async fn get(&self, key: &str) -> redis::RedisResult<Option<Vec<u8>>> {
        let mut conn = self.conn.lock().await;
        conn.get(key).await
    }

    pub async fn insert_with_ttl(&self, key: &str, value: &[u8], ttl_secs: usize, max: usize) -> redis::RedisResult<()> {
        let mut conn = self.conn.lock().await;
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
        Ok(())
    }
}
