use askama::Template;
use askama_axum::IntoResponse;
use axum::{
    extract::{DefaultBodyLimit, Multipart, Query, Request, State},
    http::{
        header::{CACHE_CONTROL, SET_COOKIE},
        HeaderMap, Response, StatusCode,
    },
    middleware::{self, Next},
    response::Redirect,
    routing::{get, post},
    Router,
};
use axum_extra::headers::{Cookie, HeaderMapExt};
use axum_typed_multipart::{TryFromMultipart, TypedMultipart};
use image::{
    codecs::{avif::AvifEncoder, jpeg::JpegEncoder, webp::WebPEncoder},
    ImageReader,
};
use mime_guess::mime::IMAGE;
use moka::future::{Cache, CacheBuilder};
use serde::{Deserialize, Serialize};
use std::{
    env::{self},
    fmt,
    fs::remove_file, io::{Cursor, Seek},
};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};
use tower_http::services::ServeDir;
use uuid::Uuid;
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};
#[derive(TryFromMultipart)]
struct LoginRequest {
    password: String,
}
#[derive(Serialize, Deserialize)]
struct LoginQueryParams {
    redirect: Option<String>,
}
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
enum Encoding {
    WebP,
    AVIF,
    JPEG,
}
impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
        // or, alternatively:
        // fmt::Debug::fmt(self, f)
    }
}
#[derive(Serialize, Deserialize)]
struct FileParams {
    width: Option<u32>,
    height: Option<u32>,
    #[serde(default = "default_resource")]
    encoding: Encoding,
}

fn default_resource() -> Encoding {
    Encoding::JPEG
}

#[derive(Clone)]
struct AppState {
    cache: Cache<String, Vec<u8>>,
}
enum AppError {
    MissingMimeType,
}
impl IntoResponse for AppError {
    fn into_response(self) -> Response<axum::body::Body> {
        // How we want errors responses to be serialized
        return (StatusCode::INTERNAL_SERVER_ERROR, UploadForm {}).into_response();
    }
}
async fn create_zip() -> Result<impl IntoResponse, AppError> {
    let bytes: Vec<u8> = zip_dir();

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", "application/zip".parse().unwrap());
    headers.insert(
        "Content-Disposition",
        "attachment; filename=\"Hochzeitsbilder_28-09-2024.zip\""
            .parse()
            .unwrap(),
    );
    Ok((headers, bytes))
}
fn zip_dir() -> Vec<u8> {
    let data_dir: String = env::var("DATA_DIR").expect("$DATA_DIR is not set");

    let paths = fs::read_dir(&data_dir).unwrap();
    let  cursor = & mut Cursor::new(vec![]);
    let mut zip = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default();

    let prefix = Path::new(&data_dir);
    let mut buffer = Vec::new();
    for entry in paths {
        let path = entry.unwrap().path();
        let name = path.strip_prefix(prefix).unwrap();
        let path_as_string = name.to_str().map(str::to_owned).unwrap();
        // Write file or directory explicitly
        // Some unzip tools unzip files with directory paths correctly, some do not!
        if path.is_file() {
            zip.start_file(path_as_string, options).unwrap();
            let mut f = File::open(path).unwrap();
            f.read_to_end(&mut buffer).unwrap();
            zip.write_all(&buffer).unwrap();
            buffer.clear();
        } else if !name.as_os_str().is_empty() {
            // Only if not root! Avoids path spec / warning
            // and mapname conversion failed error on unzip
            zip.add_directory(path_as_string, options).unwrap();
        }
    }
    let zipped=zip.finish().unwrap();
    zipped.flush().unwrap();
    zipped.set_position(0);
    let mut res_data = Vec::new();
    zipped.read_to_end(&mut res_data).unwrap();
    return res_data;
}
async fn upload(mut multipart: Multipart) -> Redirect {
    let data_dir: String = env::var("DATA_DIR").expect("$DATA_DIR is not set");
    while let Some(field) = multipart.next_field().await.unwrap() {
        let file_name = field.file_name().unwrap().to_string();
        let file_ending = file_name.split(".").last().unwrap_or("unknown");
        let uuid = Uuid::new_v4();
        let data = field.bytes().await.unwrap();
        let original_filename = &format!("{}/{}.{}", data_dir, uuid, file_ending);
        let path = Path::new(original_filename);
        let display = path.display();

        let mut file = match File::create(&path) {
            Err(why) => panic!("couldn't create {}: {}", display, why),
            Ok(file) => file,
        };

        file.write_all(&data).expect("failed to write");
    }
    Redirect::to("/gallery")
}

#[derive(Template)]
#[template(path = "Home.html")]
struct Home {}
#[derive(Template)]
#[template(path = "Login.html")]
struct LoginForm {}
#[derive(Template)]
#[template(path = "Upload.html")]
struct UploadForm {}
#[derive(Template)]
#[template(path = "Gallery.html")]
struct Gallery {
    file_paths: Vec<String>,
}

async fn show_gallery() -> Gallery {
    let data_dir: String = env::var("DATA_DIR").expect("$DATA_DIR is not set");

    let paths = fs::read_dir(&data_dir).unwrap();
    Gallery {
        file_paths: paths
            .into_iter()
            .map(|path| path.unwrap().path().to_str().unwrap().into())
            .collect(),
    }
}

async fn login(
    Query(login_query_params): Query<LoginQueryParams>,
    login_request: TypedMultipart<LoginRequest>,
) -> Result<impl IntoResponse, AppError> {
    let expected_password: String = env::var("PASSWORD").expect("$PASSWORD is not set");
    let next_page = login_query_params.redirect.unwrap_or("/".to_string());
    let password = login_request.password.clone();
    let mut headers = HeaderMap::new();

    if expected_password.eq(&password) {
        let access_token_cookie = format!("password={}; SameSite=Lax; Path=/; HttpOnly", password);
        // Set cookie
        headers.insert(SET_COOKIE, access_token_cookie.parse().unwrap());
        return Ok((headers, Redirect::to(&next_page)));
    }
    Ok((
        headers,
        Redirect::to(&format!("/login?redirect={}", next_page)),
    ))
}
async fn auth(req: Request, next: Next) -> Result<axum::response::Response, StatusCode> {
    if req.uri().path().starts_with("/login") || req.uri().path().starts_with("/assets") {
        return Ok(next.run(req).await);
    }
    if let Some(cookie) = req.headers().typed_get::<Cookie>() {
        let password: String = env::var("PASSWORD").expect("$PASSWORD is not set");
        if let Some(password_header) = cookie.get("password") {
            if password_header.eq(&password) {
                return Ok(next.run(req).await);
            }
        }
    }
    return Ok(Redirect::to(&format!("/login?redirect={}", &req.uri().path())).into_response());
}
async fn show_upload() -> UploadForm {
    UploadForm {}
}
async fn show_login() -> LoginForm {
    LoginForm {}
}
async fn show_home() -> Home {
    Home {}
}
const ONE_WEEK_IN_SECONDS: u32 = 604800;
async fn get_file(
    State(state): State<AppState>,
    axum::extract::Path(file_name): axum::extract::Path<String>,
    Query(file_params): Query<FileParams>,
) -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        CACHE_CONTROL,
        format!("max-age={}", ONE_WEEK_IN_SECONDS).parse().unwrap(),
    );
    let data_dir: String = env::var("DATA_DIR").expect("$DATA_DIR is not set");
    let original_filename = &format!("{}/{}", data_dir, &file_name);
    let mime_guess = mime_guess::from_path(&file_name);
    let cache = state.cache.clone();
    if let Some(mime) = mime_guess.first() {
        if mime.type_() == IMAGE {
            let image = ImageReader::open(original_filename)
                .unwrap()
                .decode()
                .unwrap();

            let new_width = file_params.width.unwrap_or(image.width());
            let new_height = file_params.height.unwrap_or(image.height());
            let key = format!(
                "{}-{}-{}-{}",
                &file_name, new_width, new_height, file_params.encoding
            );
            if cache.contains_key(&key) {
                return (headers, cache.get(&key).await.unwrap());
            }
            let resized_image = image.thumbnail(new_width, new_height);

            let mut default = vec![];

            match file_params.encoding {
                Encoding::WebP => resized_image
                    .write_with_encoder(WebPEncoder::new_lossless(&mut default))
                    .unwrap(),
                Encoding::AVIF => resized_image
                    .write_with_encoder(AvifEncoder::new_with_speed_quality(&mut default, 5, 80)) //should not be used for now, isvery slow
                    .unwrap(),
                Encoding::JPEG => resized_image
                    .write_with_encoder(JpegEncoder::new_with_quality(&mut default, 85))
                    .unwrap(),
            };

            cache.insert(key, default.clone()).await;
            return (headers, default);
        }
    }
    let mut buffer = vec![];
    let mut f = File::open(original_filename).unwrap();
    f.read(&mut buffer).unwrap();
    return (headers, buffer);
}
#[tokio::main]
async fn main() {
    let data_dir: String = env::var("DATA_DIR").expect("$DATA_DIR is not set");
    let asset_dir: String = env::var("ASSET_DIR").expect("$ASSET_DIR is not set");

    let serve_dir_from_assets = ServeDir::new(&asset_dir);
    let serve_dir_from_files = ServeDir::new(&data_dir);

    let state = AppState {
        cache: CacheBuilder::new(10_000).build(),
    };
    let app = Router::new()
        .nest_service("/assets", serve_dir_from_assets) // .nest_service("/files", serve_dir_from_files)
        .route("/files/:file_name", get(get_file))
        .route("/login", get(show_login))
        .route("/login", post(login))
        .route("/gallery", get(show_gallery))
        .route("/zip", get(create_zip))
        .route("/upload", get(show_upload))
        .route("/", get(show_home))
        .route(
            "/upload-multi",
            post(upload).layer(DefaultBodyLimit::max(500 * 1024 * 1024)),
        )
        .layer(middleware::from_fn(auth))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
