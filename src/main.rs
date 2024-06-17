use actix_files as fs;
use actix_multipart::Multipart;
use actix_web::{web, App, HttpResponse, HttpServer, Result};
use futures_util::stream::StreamExt as _;
use std::collections::HashMap;
use std::fs::read_to_string;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::sync::Mutex;
use actix_web::web::Data;
use sled::Db;
use serde::{Deserialize, Serialize};
use rand::{distributions::Alphanumeric, Rng};
use std::collections::hash_map::DefaultHasher;
use mime_guess::MimeGuess;
use chrono::Utc; // Add this line

// Define the MIME types manually
const MIME_IMAGE_JPEG: &str = "image/jpeg";
const MIME_IMAGE_PNG: &str = "image/png";
const MIME_IMAGE_GIF: &str = "image/gif";
const MIME_IMAGE_WEBP: &str = "image/webp";
const MIME_VIDEO_MP4: &str = "video/mp4";
const MIME_AUDIO_MPEG: &str = "audio/mpeg";
const MIME_VIDEO_WEBM: &str = "video/webm";

// Maximum file size (20 MB)
const MAX_SIZE: usize = 20 * 1024 * 1024;
const POSTS_PER_PAGE: usize = 30;

#[derive(Serialize, Deserialize, Clone)]
struct Post {
    id: String,
    parent_id: String,
    title: String,
    message: String,
    file_path: Option<String>,
    last_reply_at: u64,
}

fn render_template(path: &str, context: &HashMap<&str, String>) -> String {
    let template = read_to_string(path).expect("Unable to read template file");
    let mut rendered = template;
    for (key, value) in context {
        let placeholder = format!("{{{{{}}}}}", key);
        rendered = rendered.replace(&placeholder, value);
    }
    rendered
}

fn generate_color_from_id(id: &str) -> String {
    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    let hash = hasher.finish();
    let r = (hash & 0xFF) as u8;
    let g = ((hash >> 8) & 0xFF) as u8;
    let b = ((hash >> 16) & 0xFF) as u8;
    format!("#{:02X}{:02X}{:02X}", r, g, b)
}

fn sanitize_input(input: &str) -> String {
    htmlescape::encode_minimal(input)
}

async fn save_file(mut payload: Multipart, db: web::Data<Mutex<Db>>) -> Result<HttpResponse> {
    let mut title = String::new();
    let mut message = String::new();
    let mut file_path = None;
    let mut parent_id = String::from("0");

    while let Some(item) = payload.next().await {
        let mut field = item?;
        let content_disposition = field.content_disposition().clone();
        let name = content_disposition.get_name().unwrap_or("").to_string();

        match name.as_str() {
            "title" => {
                while let Some(chunk) = field.next().await {
                    let data = chunk?;
                    title.push_str(&String::from_utf8_lossy(&data));
                }
            },
            "message" => {
                while let Some(chunk) = field.next().await {
                    let data = chunk?;
                    message.push_str(&String::from_utf8_lossy(&data));
                }
            },
            "file" => {
                if let Some(filename) = content_disposition.get_filename() {
                    let mime_type = MimeGuess::from_path(filename).first_or_octet_stream();
                    let sanitized_filename = sanitize_filename::sanitize(&filename);
                    let unique_id: String = rand::thread_rng()
                        .sample_iter(&Alphanumeric)
                        .take(6)
                        .map(char::from)
                        .collect();
                    let unique_filename = format!("{}-{}", unique_id, sanitized_filename);

                    let valid_mime_types = [
                        MIME_IMAGE_JPEG,
                        MIME_IMAGE_PNG,
                        MIME_IMAGE_GIF,
                        MIME_IMAGE_WEBP,
                        MIME_VIDEO_MP4,
                        MIME_AUDIO_MPEG,
                        MIME_VIDEO_WEBM,
                    ];

                    if valid_mime_types.contains(&mime_type.as_ref()) {
                        let file_path_string = format!("./static/{}", unique_filename);
                        let file_path_clone = file_path_string.clone();
                        let mut f = web::block(move || std::fs::File::create(file_path_clone)).await??;

                        while let Some(chunk) = field.next().await {
                            let data = chunk?;
                            f = web::block(move || f.write_all(&data).map(|_| f)).await??;
                        }

                        file_path = Some(file_path_string);
                    }
                }
            },
            "parent_id" => {
                while let Some(chunk) = field.next().await {
                    let data = chunk?;
                    parent_id = String::from_utf8_lossy(&data).trim().to_string();
                }
            },
            _ => {},
        }
    }

    let title = sanitize_input(&title);
    let message = sanitize_input(&message);

    if title.trim().is_empty() || message.trim().is_empty() {
        return Ok(HttpResponse::BadRequest().body("Title and message are mandatory."));
    }

    if title.len() > 30 || message.len() > 50000 {
        return Ok(HttpResponse::BadRequest().body("Title or message is too long."));
    }

    let post_id: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(6)
        .map(char::from)
        .collect();

    let new_post = Post {
        id: post_id.clone(),
        parent_id: parent_id.clone(),
        title,
        message,
        file_path,
        last_reply_at: Utc::now().timestamp_millis() as u64,
    };

    let db = db.lock().unwrap();
    db.insert(post_id.as_bytes(), serde_json::to_vec(&new_post).unwrap()).unwrap();

    if parent_id != "0" {
        let parent_key = parent_id.as_bytes();
        if let Ok(Some(parent_post_data)) = db.get(parent_key) {
            let mut parent_post: Post = serde_json::from_slice(&parent_post_data).unwrap();
            parent_post.last_reply_at = Utc::now().timestamp_millis() as u64;
            db.insert(parent_key, serde_json::to_vec(&parent_post).unwrap()).unwrap();
        }
    }

    if parent_id == "0" {
        Ok(HttpResponse::SeeOther().append_header(("Location", "/")).finish())
    } else {
        Ok(HttpResponse::SeeOther().append_header(("Location", format!("/post/{}", parent_id))).finish())
    }
}

async fn view_post(db: web::Data<Mutex<Db>>, path: web::Path<String>) -> Result<HttpResponse> {
    let db = db.lock().unwrap();
    let post_id = path.into_inner();

    let mut posts_html = String::new();
    let mut is_original_post = true;
    let mut reply_count = 1;

    for item in db.iter().values() {
        let post_data = item.unwrap();
        let post: Post = serde_json::from_slice(&post_data).unwrap();
        if post.id == post_id || post.parent_id == post_id {
            posts_html.push_str("<div class=\"post\">");
            if is_original_post {
                posts_html.push_str("<div class=\"post-id\">Original Post</div>");
                is_original_post = false;
            } else {
                posts_html.push_str(&format!("<div class=\"post-id\">Reply {}</div>", reply_count));
                reply_count += 1;
            }
            posts_html.push_str(&format!("<div class=\"post-title\">{}</div>", post.title));
            if let Some(file_path) = post.file_path {
                if file_path.ends_with(".jpg") || file_path.ends_with(".jpeg") || file_path.ends_with(".png") || file_path.ends_with(".gif") || file_path.ends_with(".webp") {
                    posts_html.push_str(&format!(r#"<img src="/static/{}"><br>"#, file_path.trim_start_matches("./static/")));
                } else if file_path.ends_with(".mp4") || file_path.ends_with(".mp3") || file_path.ends_with(".webm") {
                    posts_html.push_str(&format!(r#"<video controls><source src="/static/{}"></video><br>"#, file_path.trim_start_matches("./static/")));
                }
            }
            posts_html.push_str(&format!("<div class=\"post-message\">{}</div>", post.message));
            posts_html.push_str("</div>");
        }
    }

    let context = HashMap::from([
        ("PARENT_ID", post_id.clone()),
        ("POSTS", posts_html),
    ]);

    let body = render_template("templates/view_post.html", &context);

    Ok(HttpResponse::Ok().content_type("text/html").body(body))
}

async fn index(db: web::Data<Mutex<Db>>, query: web::Query<HashMap<String, String>>) -> Result<HttpResponse> {
    let db = db.lock().unwrap();
    let page: usize = query.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
    let offset = (page - 1) * POSTS_PER_PAGE;

    // Get all posts sorted by last_reply_at
    let mut posts: Vec<Post> = db
        .iter()
        .values()
        .filter_map(|item| {
            let post_data = item.ok()?;
            let post: Post = serde_json::from_slice(&post_data).ok()?;
            Some(post)
        })
        .filter(|post| post.parent_id == "0")
        .collect();

    posts.sort_by_key(|post| std::cmp::Reverse(post.last_reply_at));

    // Get the total number of posts and determine if there is a next page
    let total_posts = posts.len();
    let total_pages = (total_posts as f64 / POSTS_PER_PAGE as f64).ceil() as usize;
    let has_next_page = page < total_pages;

    let posts_html: String = posts
        .into_iter()
        .skip(offset)
        .take(POSTS_PER_PAGE)
        .map(|post| {
            let reply_count = db
                .iter()
                .values()
                .filter_map(|item| {
                    let post_data = item.ok()?;
                    let reply_post: Post = serde_json::from_slice(&post_data).ok()?;
                    Some(reply_post)
                })
                .filter(|reply_post| reply_post.parent_id == post.id)
                .count();

            let truncated_message = if post.message.len() > 2700 {
                format!("{}... <a href=\"/post/{}\" class=\"view-full-post\">Click here to open full post</a>", &post.message[..2700], post.id)
            } else {
                post.message.clone()
            };

            let post_color = generate_color_from_id(&post.id);

            format!(
                "<div class=\"post\">
                    <div class=\"post-id-box\" style=\"background-color: {}\">{}</div>
                    <div class=\"post-title title-green\">{}</div>
                    {}
                    <div class=\"post-message\">{}</div>
                    <a class=\"reply-button\" href=\"/post/{}\">Reply ({})</a>
                </div>",
                post_color,
                post.id,
                post.title,
                if let Some(file_path) = post.file_path {
                    if file_path.ends_with(".jpg") || file_path.ends_with(".jpeg") || file_path.ends_with(".png") || file_path.ends_with(".gif") || file_path.ends_with(".webp") {
                        format!(r#"<img src="/static/{}"><br>"#, file_path.trim_start_matches("./static/"))
                    } else if file_path.ends_with(".mp4") || file_path.ends_with(".mp3") || file_path.ends_with(".webm") {
                        format!(r#"<video controls><source src="/static/{}"></video><br>"#, file_path.trim_start_matches("./static/"))
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                },
                truncated_message,
                post.id,
                reply_count
            )
        })
        .collect();

    let next_page = if has_next_page { Some(page + 1) } else { None };
    let prev_page = if page > 1 { Some(page - 1) } else { None };
    let mut pagination_html = String::new();
    if let Some(prev) = prev_page {
        pagination_html.push_str(&format!(r#"<a href="/?page={}">Previous</a>"#, prev));
    }
    if let Some(next) = next_page {
        pagination_html.push_str(&format!(r#"<a href="/?page={}">Next</a>"#, next));
    }

    let context = HashMap::from([
        ("POSTS", posts_html),
        ("PAGINATION", pagination_html),
    ]);

    let body = render_template("templates/index.html", &context);

    Ok(HttpResponse::Ok().content_type("text/html").body(body))
}

fn initialize_db() -> Db {
    sled::open("my_database").expect("Failed to open database")
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let db = initialize_db();
    let db_data = Data::new(Mutex::new(db));

    HttpServer::new(move || {
        App::new()
            .app_data(db_data.clone())
            .app_data(Data::new(web::JsonConfig::default().limit(MAX_SIZE)))
            .service(
                web::resource("/")
                    .route(web::get().to(index))
            )
            .service(
                web::resource("/upload")
                    .route(web::post().to(save_file))
            )
            .service(
                web::resource("/post/{id}")
                    .route(web::get().to(view_post))
            )
            .service(fs::Files::new("/static", "./static").show_files_listing())
    })
    .bind("0.0.0.0:8081")?
    .run()
    .await
}
