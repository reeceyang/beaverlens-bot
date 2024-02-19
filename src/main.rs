use dotenvy::dotenv;
use futures::TryStreamExt;
use mongodb::{
    bson::{doc, DateTime},
    Client, Collection, Database,
};
use nescookie;
use poise::serenity_prelude::{
    self as serenity, async_trait, ChannelId, CreateMessage, EventHandler, GuildId, Message, Ready,
};
use serde::{Deserialize, Serialize};
use std::{
    env,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
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

#[derive(Serialize, Deserialize)]
struct MaxPostNumber {
    number: u32,
}

#[derive(Serialize, Deserialize)]
struct DiscordChannel {
    channel_id: String,
}

const FACEBOOK_ROOT_URL: &str = "https://facebook.com/";

async fn get_new_posts(
    parse_until: u32,
    cookie_jar: cookie::CookieJar,
) -> WebDriverResult<Vec<Confession>> {
    let mut caps = DesiredCapabilities::chrome();
    caps.set_headless()?;

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

async fn insert_posts(database: &Database, posts: &Vec<Confession>) -> mongodb::error::Result<()> {
    let collection: Collection<Confession> =
        database.collection(env::var("MONGODB_POSTS_COLLECTION").unwrap().as_str());

    collection.insert_many(posts, None).await?;
    Ok(())
}

async fn post_confessions(
    ctx: &poise::serenity_prelude::Context,
    posts: &Vec<Confession>,
) -> Result<(), Error> {
    let channels = get_all_discord_channels()
        .await
        .expect("failed to get channels");
    for channel in channels.iter() {
        let channel_id: u64 = channel.channel_id.parse().unwrap();
        for post in posts.iter().rev() {
            let no_number_text = post.post_text.split_once(" ").unwrap().1;
            let message = CreateMessage::new().content(format!(
                "**#{}** {}\n<{}{}>",
                post.number, no_number_text, FACEBOOK_ROOT_URL, post.post_id
            ));
            ChannelId::new(channel_id)
                .send_message(&ctx, message)
                .await?;
        }
    }
    Ok(())
}

async fn get_max_post_number(database: &Database) -> mongodb::error::Result<u32> {
    let collection: Collection<MaxPostNumber> = database.collection(
        env::var("MONGODB_MAX_POST_NUMBER_COLLECTION")
            .unwrap()
            .as_str(),
    );

    Ok(collection.find_one(doc! {}, None).await?.unwrap().number)
}

async fn update_max_post_number(
    database: &Database,
    MaxPostNumber { number }: MaxPostNumber,
) -> mongodb::error::Result<()> {
    let collection: Collection<MaxPostNumber> = database.collection(
        env::var("MONGODB_MAX_POST_NUMBER_COLLECTION")
            .unwrap()
            .as_str(),
    );

    collection
        .find_one_and_update(
            doc! {},
            doc! {
            "$set":
                {
                    "number": number,
                }
            },
            None,
        )
        .await?;

    Ok(())
}

async fn add_discord_channel(
    DiscordChannel { channel_id }: DiscordChannel,
) -> mongodb::error::Result<()> {
    let database = get_new_database().await.expect("failed to get database");
    let collection: Collection<DiscordChannel> =
        database.collection(env::var("MONGODB_CHANNELS_COLLECTION").unwrap().as_str());

    let existing_channel = collection
        .find_one(
            doc! {
                "channel_id": channel_id.to_string(),
            },
            None,
        )
        .await?;

    match existing_channel {
        Some(_) => (), // channel already exists
        None => {
            collection
                .insert_one(DiscordChannel { channel_id }, None)
                .await?;
        }
    }

    Ok(())
}

async fn remove_discord_channel(
    DiscordChannel { channel_id }: DiscordChannel,
) -> mongodb::error::Result<()> {
    let database = get_new_database().await.expect("failed to get database");
    let collection: Collection<DiscordChannel> =
        database.collection(env::var("MONGODB_CHANNELS_COLLECTION").unwrap().as_str());

    collection
        .delete_one(
            doc! {
                "channel_id": channel_id.to_string(),
            },
            None,
        )
        .await?;

    Ok(())
}

async fn get_all_discord_channels() -> mongodb::error::Result<Vec<DiscordChannel>> {
    let database = get_new_database().await.expect("failed to get database");
    let collection: Collection<DiscordChannel> =
        database.collection(env::var("MONGODB_CHANNELS_COLLECTION").unwrap().as_str());

    let cursor = collection.find(doc! {}, None).await?;

    let all_channels: Vec<DiscordChannel> = cursor.try_collect().await?;

    Ok(all_channels)
}

async fn get_new_database() -> Result<Database, Error> {
    let uri = env::var("MONGODB_CONNECTION_STRING").unwrap();
    let client = Client::with_uri_str(uri).await?;
    Ok(client.database(env::var("MONGODB_DATABASE").unwrap().as_str()))
}

async fn check_and_process_new_confessions(
    ctx: &poise::serenity_prelude::Context,
) -> Result<(), Error> {
    let jar = nescookie::open(env::var("FACEBOOK_COOKIES_FILE").unwrap()).unwrap();

    let database = get_new_database().await?;

    let parse_until = get_max_post_number(&database).await? + 1;
    let posts = get_new_posts(parse_until, jar).await.unwrap();

    if posts.is_empty() {
        return Ok(());
    }

    post_confessions(&ctx, &posts).await?;
    insert_posts(&database, &posts).await?;

    match posts.iter().map(|post| post.number).max() {
        None => (),
        Some(new_max_number) => {
            update_max_post_number(
                &database,
                MaxPostNumber {
                    number: new_max_number,
                },
            )
            .await?
        }
    }

    Ok(())
}

struct Data {} // User data, which is stored and accessible in all command invocations
type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

#[poise::command(slash_command)]
async fn set_confess_channel(ctx: Context<'_>) -> Result<(), Error> {
    let channel_name = ctx.channel_id().name(ctx.http()).await?;
    add_discord_channel(DiscordChannel {
        channel_id: ctx.channel_id().to_string(),
    })
    .await?;

    let response = format!("new confessions will be posted in #{}", channel_name);
    ctx.say(response).await?;
    Ok(())
}

#[poise::command(slash_command)]
async fn remove_confess_channel(ctx: Context<'_>) -> Result<(), Error> {
    let channel_name = ctx.channel_id().name(ctx.http()).await?;
    remove_discord_channel(DiscordChannel {
        channel_id: ctx.channel_id().to_string(),
    })
    .await?;

    let response = format!("new confessions will not be posted in #{}", channel_name);
    ctx.say(response).await?;
    Ok(())
}

struct Handler {
    is_loop_running: AtomicBool,
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: poise::serenity_prelude::Context, msg: Message) {
        if msg.content.starts_with("!ping") {
            if let Err(why) = msg.channel_id.say(&ctx.http, "Pong!").await {
                eprintln!("Error sending message: {why:?}");
            }
        }
    }

    async fn ready(&self, _ctx: poise::serenity_prelude::Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
    }

    // We use the cache_ready event just in case some cache operation is required in whatever use
    // case you have for this.
    async fn cache_ready(&self, ctx: poise::serenity_prelude::Context, _guilds: Vec<GuildId>) {
        println!("Cache built successfully!");

        // It's safe to clone Context, but Arc is cheaper for this use case.
        // Untested claim, just theoretically. :P
        let ctx = Arc::new(ctx);

        // We need to check that the loop is not already running when this event triggers, as this
        // event triggers every time the bot enters or leaves a guild, along every time the ready
        // shard event triggers.
        //
        // An AtomicBool is used because it doesn't require a mutable reference to be changed, as
        // we don't have one due to self being an immutable reference.
        if !self.is_loop_running.load(Ordering::Relaxed) {
            // We have to clone the Arc, as it gets moved into the new thread.
            let ctx1 = Arc::clone(&ctx);
            // tokio::spawn creates a new green thread that can run in parallel with the rest of
            // the application.
            tokio::spawn(async move {
                loop {
                    check_and_process_new_confessions(&ctx1)
                        .await
                        .expect("checking for or processing new confessions failed");
                    tokio::time::sleep(Duration::from_secs(1200)).await;
                }
            });

            // Now that the loop is running, we set the bool to true
            self.is_loop_running.swap(true, Ordering::Relaxed);
        }
    }
}

#[tokio::main]
async fn main() {
    dotenv().expect(".env file not found");

    let token = std::env::var("DISCORD_TOKEN").expect("missing DISCORD_TOKEN");
    let intents = serenity::GatewayIntents::non_privileged();

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![set_confess_channel(), remove_confess_channel()],
            ..Default::default()
        })
        .setup(|ctx, _ready, framework| {
            Box::pin(async move {
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                Ok(Data {})
            })
        })
        .build();

    let client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .event_handler(Handler {
            is_loop_running: AtomicBool::new(false),
        })
        .await;
    client.unwrap().start().await.unwrap();
}
