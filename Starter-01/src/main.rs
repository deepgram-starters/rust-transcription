#[macro_use] extern crate rocket;

use rocket::{Rocket, Build};
use rocket::fairing::AdHoc;
use rocket::request::FlashMessage;
use rocket::serde::{Serialize, Deserialize};
use rocket::serde::json::Json;
use serde_json::Value;
use rocket::form::Form;
use rocket::config::{Config};
use rocket::fs::{FileServer, relative, TempFile};
use reqwest::{Client, Error};
use std::env;
use dotenv::dotenv;

#[derive(FromForm)]
struct Request<'f> {
    url: Option<String>,
    features: Option<String>,
    model: Option<String>,
    version: Option<String>,
    tier: Option<String>,
    file: Option<TempFile<'f>>,
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
    let dg_features: serde_json::Value = serde_json::from_str(&features).expect("Failed to parse features");

    let body_data = serde_json::json!({
        "url": url
    });

    let client = reqwest::Client::new();
    let endpoint = "https://api.deepgram.com/v1/listen";
    let query_str = serde_urlencoded::to_string(&dg_features).expect("Failed to encode url");
    let response = client.post(format!("{}?{}", endpoint, query_str))
        .header("Authorization", format!("token {}", api_key))
        .json(&body_data)
        .send()
        .await;

    let transcription: serde_json::Value = response.unwrap().json().await.expect("Failed to parse JSON");
    let res_data = Response {
        model: Some(model),
        version: Some(version),
        tier: Some(tier),
        dgFeatures: dg_features,
        transcription: transcription,
    };
                   
    println!("{:?}", query_str);
    Json(res_data)
}
        


#[launch]
fn rocket() -> _ {
    let port = env::var("port")
    .unwrap_or_else(|_| "8080".to_string())
    .parse::<u16>()
    .expect("Invalid port number");

    let figment = rocket::Config::figment()
    .merge(("port", port));

    rocket::custom(figment)
    .mount("/", routes![transcribe])
    .mount("/", FileServer::from(relative!("static")))
        
}