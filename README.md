# cdn-rs

Высоконагруженный CDN-сервис на **Rust + Actix** с Postgres и ресайзом изображений (crate `image`), полностью совместимый по эндпоинтам с исходным FastAPI:

- `POST /images/` — приём base64 и сохранение оригинала (JPEG)
- `GET  /{img_guid}/{parametr_images}` — отдача JPEG по размеру `HxW` (лимит 1600)
- `GET  /{img_guid}/` — отдача оригинала (из файла, или автодокачка по `link_o`)
- `GET  /{img_guid}.{format}?odnWidth=&odnHeight=&odnBg=` — отдача в выбранном формате (`JPEG|PNG|GIF|TIFF|WebP`)
- дублированные пути с префиксом `/images/...` — **удар в удар** с Python-реализацией

API-документация (Swagger UI) доступна на `/docs` (вкл/выкл через `.env`).

---

## Архитектура

```
cdn-rs/
├─ Cargo.toml
├─ .env.example
├─ README.md
├─ Dockerfile
├─ docker-compose.yml    # опционально
├─ .dockerignore
├─ migrations/
│  └─ 001_create_images.sql
└─ src/
   ├─ main.rs
   ├─ config.rs
   ├─ errors.rs
   ├─ db.rs
   ├─ proxy.rs
   ├─ util.rs
   ├─ imaging.rs
   ├─ openapi.rs
   ├─ routes/
   │  └─ cdn.rs
   └─ models/
      └─ image.rs
```

### Ключевые моменты производительности

- **Actix** + фиксированные воркеры по числу CPU (не меньше 4).
- CPU-тяжёлые операции ресайза выполняются в `spawn_blocking`, не блокируя реактор.
- `sqlx` c пулом соединений (по умолчанию до 40).
- Быстрый I/O, минимизация копий; контент отдаётся напрямую в ответ.
- Ленивое создание директорий и idempotent запись файлов.
- Возможность скачивания оригинала по `link_o` при первом запросе.

---

## Требования

- Linux/WSL2/macOS
- Docker / Docker Compose (для контейнерного запуска) либо Rust 1.79+
- Postgres 13+ (в `docker-compose.yml` уже есть готовый сервис)

---

## Быстрый старт (Docker Compose)

1. Скопируйте пример окружения и поправьте значения:
   ```bash
   cp .env.example .env
   ```

2. (Опционально) создайте локальную папку под медиа:
   ```bash
   mkdir -p ./media
   ```

3. Поднимите сервисы:
   ```bash
   docker compose up -d --build
   ```

4. Проверьте сервис:
   - Swagger: http://localhost:8080/docs
   - OpenAPI JSON: http://localhost:8080/api-docs/openapi.json

База данных поднимется автоматически, таблица создастся при применении миграции (см. ниже).

### Переменные окружения

| Переменная | Описание | Значение по умолчанию |
|-----------|----------|-----------------------|
| `BIND_ADDR` | Адрес/порт HTTP-сервера | `0.0.0.0:8080` |
| `MEDIA_BASE_DIR` | Путь хранения файлов (аналог `castom_addres`) | `/var/www/onlihub/data/www/onlihub-media.com/` |
| `NO_IMAGE_FILE` | Имя файла-заглушки внутри `MEDIA_BASE_DIR` | `no-image-01.jpg` |
| `MAX_IMAGE_SIDE` | Лимит на сторону при ресайзе | `1600` |
| `DATABASE_URL` | Строка подключения к Postgres | — (обязательна) |
| `USE_CACHE` | Включить Redis-кэш (`true`/`false`) | `false` |
| `REDIS_HOST` | Хост Redis | `127.0.0.1` |
| `REDIS_PORT` | Порт Redis | `6379` |
| `REDIS_DB` | Номер базы Redis | `0` |
| `REDIS_PASSWORD` | Пароль Redis | _пусто_ |
| `SWAGGER_ENABLED` | Включить Swagger UI (`true`/`false`) | `true` |
| `SWAGGER_TITLE` | Заголовок для Swagger | `Onlihub Media CDN` |
| `SWAGGER_VERSION` | Версия API в Swagger | `1.0.0` |
| `USE_PROXIES` | Включить случайные HTTP(S) прокси при скачивании оригиналов (`true/1`) | `false` |

### Миграция

```sql
create table if not exists images (
  id bigserial primary key,
  guid uuid not null unique,
  link_o text,
  status int,
  status_date timestamptz
);

create index if not exists images_guid_idx on images(guid);
create index if not exists images_link_o_idx on images(link_o);
```

