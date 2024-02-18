use dotenvy::dotenv;
use mongodb::{bson::DateTime, Client, Collection};
use nescookie;
use serde::{Deserialize, Serialize};
use std::env;
use thirtyfour::{
    cookie::{self, SameSite},
    error::WebDriverResult,
    extensions::query::ElementQueryable,
    By, DesiredCapabilities, WebDriver,
};
#[derive(Serialize, Deserialize)]
struct Confession {
    post_text: String,
    time: DateTime,
    timestamp: i32,
    post_url: String,
    post_id: String,
    number: u32,
}

const FACEBOOK_ROOT_URL: &str = "https://facebook.com/";

async fn get_new_posts(cookie_jar: cookie::CookieJar) -> WebDriverResult<Vec<Confession>> {
    let caps = DesiredCapabilities::chrome();
    let driver = WebDriver::new("http://localhost:9515", caps).await?;

    driver
        .goto("https://facebook.com/beaverconfessions")
        .await?;

    for cookie in cookie_jar.iter() {
        let mut lax_cookie = cookie.clone();
        lax_cookie.set_same_site(Some(SameSite::Lax));
        driver.add_cookie(lax_cookie).await?;
    }

    driver.refresh().await?;
    let parse_until: u32 = 71318;
    let mut earliest_seen: u32 = u32::MAX;
    let mut num_seen = 0;

    let mut new_confessions: Vec<Confession> = vec![];

    while earliest_seen > parse_until {
        let all_actions_buttons = driver
            .query(By::Css("[aria-label=\"Actions for this post\"]"))
            .all()
            .await?;

        println!("found {} actions buttons", all_actions_buttons.len());
        if all_actions_buttons.len() == num_seen {
            // failed to load more posts for some reason
            break;
        }

        for actions_button in all_actions_buttons.iter().skip(num_seen) {
            if !actions_button.is_clickable().await? {
                continue;
            }
            actions_button.click().await?;

            let embed_button = driver
                .query(By::XPath("//*[text() = 'Embed']"))
                .first()
                .await?;

            embed_button.click().await?;

            let _ = driver
                .query(By::Css("iframe[allowfullscreen]"))
                .first()
                .await?
                .enter_frame()
                .await?;

            let post_message = driver
                .query(By::Css(".userContent"))
                .first()
                .await?
                .text()
                .await?;

            let post_number = post_message
                .split_once(" ")
                .unwrap()
                .0
                .chars()
                .skip(1)
                .collect::<String>()
                .parse::<u32>()
                .unwrap();

            earliest_seen = earliest_seen.min(post_number);

            // TODO: is this still needed?
            if post_number > earliest_seen {
                continue;
            }

            if post_number < parse_until {
                break;
            }

            let timestamp_seconds = driver
                .query(By::Css("abbr.timestamp"))
                .first()
                .await?
                .attr("data-utime")
                .await?
                .expect("data-utime attr had no value")
                .parse::<i64>()
                .unwrap();

            let relative_post_url = driver
                .query(By::Css("a:has(abbr.timestamp)"))
                .first()
                .await?
                .attr("href")
                .await?
                .expect("href attr had no value")
                .replace("?ref=embed_post", "");

            let post_id = relative_post_url.rsplit_once("/").unwrap().1.to_string();

            driver.enter_parent_frame().await?;

            driver
                .query(By::Css("[aria-label=\"Close\"]"))
                .first()
                .await?
                .click()
                .await?;

            println!("timestamp: {}", timestamp_seconds);
            println!("post text: {}", post_message);
            println!("post number: {}", post_number);
            const MILLIS_IN_ONE_SECOND: i64 = 1000;
            new_confessions.push(Confession {
                post_text: post_message,
                time: DateTime::from_millis(timestamp_seconds * MILLIS_IN_ONE_SECOND),
                timestamp: timestamp_seconds as i32,
                post_id: post_id,
                post_url: format!("{}{}", FACEBOOK_ROOT_URL, relative_post_url),
                number: post_number,
            })
        }
        num_seen = all_actions_buttons.len();
    }

    // Always explicitly close the browser.
    driver.quit().await?;

    Ok(new_confessions)
}

async fn insert_posts(posts: Vec<Confession>) -> mongodb::error::Result<()> {
    let uri = env::var("MONGODB_CONNECTION_STRING").unwrap();
    // Create a new client and connect to the server
    let client = Client::with_uri_str(uri).await?;

    let database = client.database(env::var("MONGODB_DATABASE").unwrap().as_str());
    let collection: Collection<Confession> =
        database.collection(env::var("MONGODB_POSTS_COLLECTION").unwrap().as_str());

    collection.insert_many(posts, None).await?;
    Ok(())
}

#[tokio::main]
async fn main() {
    dotenv().expect(".env file not found");
    let jar = nescookie::open(env::var("FACEBOOK_COOKIES_FILE").unwrap()).unwrap();

    let new_posts = get_new_posts(jar).await.unwrap();

    insert_posts(new_posts).await.expect("mongodb failed");
}
