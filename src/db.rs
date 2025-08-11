use crate::errors::ApiError;
use chrono::{DateTime, Utc};
use sqlx::{postgres::PgPoolOptions, PgPool};
use uuid::Uuid;

#[derive(Clone)]
pub struct Db {
    pub pool: PgPool,
}

#[derive(sqlx::FromRow, Debug, Clone)]
#[allow(dead_code)]
pub struct ImageRow {
    pub id: i64,
    pub guid: Uuid,
    pub link_o: Option<String>,
    pub status: Option<i32>,
    pub status_date: Option<DateTime<Utc>>,
}

impl Db {
    pub async fn connect(db_url: &str) -> Result<Self, ApiError> {
        let pool = PgPoolOptions::new()
            .max_connections(40)
            .connect(db_url)
            .await
            .map_err(|e| ApiError::Db(e.to_string()))?;
        Ok(Self { pool })
    }

    pub async fn insert_image_with_status(&self, guid: Uuid, status: i32) -> Result<i64, ApiError> {
        let rec: (i64,) = sqlx::query_as(
            "insert into images (guid, status, status_date) values ($1, $2, now()) returning id",
        )
        .bind(guid)
        .bind(status)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| ApiError::Db(e.to_string()))?;
        Ok(rec.0)
    }

    pub async fn update_image_status(&self, id: i64, status: i32) -> Result<(), ApiError> {
        sqlx::query("update images set status = $1, status_date = now() where id = $2")
            .bind(status)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| ApiError::Db(e.to_string()))?;
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn get_image_by_guid(&self, guid: Uuid) -> Result<Option<ImageRow>, ApiError> {
        let rec = sqlx::query_as::<_, ImageRow>(
            "select id, guid, link_o, status, status_date from images where guid = $1",
        )
        .bind(guid)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ApiError::Db(e.to_string()))?;
        Ok(rec)
    }

    pub async fn get_original_link_by_guid(&self, guid: Uuid) -> Result<Option<String>, ApiError> {
        let rec: Option<(Option<String>,)> =
            sqlx::query_as("select link_o from images where guid = $1")
                .bind(guid)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| ApiError::Db(e.to_string()))?;
        Ok(rec.and_then(|r| r.0))
    }

    /// get or create by link_o; returns (id, guid)
    #[allow(dead_code)]
    pub async fn get_or_create_by_link(&self, link: &str) -> Result<(i64, Uuid), ApiError> {
        if let Some((id, guid)) =
            sqlx::query_as::<_, (i64, Uuid)>("select id, guid from images where link_o = $1")
                .bind(link)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| ApiError::Db(e.to_string()))?
        {
            return Ok((id, guid));
        }

        let guid = Uuid::new_v4();
        let rec: (i64,) = sqlx::query_as(
            "insert into images (guid, link_o, status, status_date) values ($1, $2, 1, now()) returning id",
        )
            .bind(guid)
            .bind(link)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| ApiError::Db(e.to_string()))?;
        Ok((rec.0, guid))
    }
}
