#[macro_use] extern crate rocket;

use rocket::serde::{Serialize, Deserialize};
use rocket::serde::json::Json;
use rocket::form::Form;
use rocket::config::Config;
use rocket::fs::{FileServer, relative, TempFile};
use reqwest::Client;
use std::env;
use dotenv::dotenv;

#[derive(FromForm)]
struct Request<'f> {
    url: Option<String>,
    file: Option<TempFile<'f>>,
    model: Option<String>,
    version: Option<String>,
    tier: Option<String>,
    features: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    model: Option<String>,
    version: Option<String>,
    tier: Option<String>,
    dgFeatures: serde_json::Value,
    transcription: serde_json::Value,
}

#[post("/api", format="multipart/form-data", data="<request>")]
async fn transcribe(request: Form<Request<'_>>) ->Json<Response> {
    dotenv().ok();
    let api_key = env::var("deepgram_api_key").expect("deepgram api_key is not set");
    
    let url = request.url.clone().unwrap_or_default();
    let model = request.model.clone().unwrap_or_default();
    let version = request.version.clone().unwrap_or_default();
    let tier = request.tier.clone().unwrap_or_default();
    let features = request.features.clone().unwrap_or_default();

    let mut dg_features: serde_json::Value = serde_json::from_str(&features).expect("Failed to parse features");

    dg_features["model"] = serde_json::Value::String(model.clone());

    if !version.is_empty() {
        dg_features["version"] = serde_json::Value::String(version.clone());
    }

    if model != "whisper" {
        dg_features["tier"] = serde_json::Value::String(tier.clone());
    }

    let mut body_data = serde_json::json!({});

    if url.starts_with("https://res.cloudinary.com/deepgram") {
        body_data = serde_json::json!({
            "url": url
        });
    } else if let Some(file) = &request.file {
        // TODO: fix file upload
    }
    
    let client = Client::new();
    let endpoint = "https://api.deepgram.com/v1/listen";
    let query_str = serde_urlencoded::to_string(&dg_features).expect("Failed to encode url");
    let response = client.post(format!("{}?{}", endpoint, query_str))
        .header("Authorization", format!("token {}", api_key))
        .json(&body_data)
        .send()
        .await;
    
    let transcription: serde_json::Value = response.unwrap().json().await.expect("Failed to parse JSON from Deepgram");
    let res_data = Response {
        model: Some(model),
        version: Some(version),
        tier: Some(tier),
        dgFeatures: dg_features,
        transcription: transcription,
    };
                   
    Json(res_data)
}
        


#[launch]
fn rocket() -> _ {
    let port = env::var("port")
    .unwrap_or_else(|_| "8080".to_string())
    .parse::<u16>()
    .expect("Invalid port number");

    let figment = Config::figment()
    .merge(("port", port));

    rocket::custom(figment)
    .mount("/", routes![transcribe])
    .mount("/", FileServer::from(relative!("static")))
        
}