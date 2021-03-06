use bson::to_bson;
use clap::Parser;
use log::{debug, error, info, warn};
use mongodb::Database;
use snafu::ResultExt;
use std::{path::PathBuf, time::Duration};
use tokio::{process::Command, time::timeout};

use crate::{
    command::{self, migrate::DB_VERSION},
    config, error,
    model::BowerbirdMetadata,
};

#[derive(Parser)]
#[clap(version)]
struct Main {
    #[clap(short, long)]
    config: Option<String>,
    #[clap(subcommand)]
    subcommand: SubcommandMain,
}

#[derive(Parser)]
enum SubcommandMain {
    Pixiv(Pixiv),
    Init,
    Migrate,
    Serve,
}

#[derive(Parser)]
struct Pixiv {
    #[clap(short, long)]
    limit: Option<u32>,
    #[clap(short, long)]
    user_id: Option<i32>,
    #[clap(subcommand)]
    subcommand: SubcommandPixiv,
}

#[derive(Parser)]
enum SubcommandPixiv {
    Illust(PixivIllust),
    Novel(PixivNovel),
}

#[derive(Parser)]
struct PixivIllust {
    #[clap(subcommand)]
    subcommand: SubcommandPixivAction,
}

#[derive(Parser)]
struct PixivNovel {
    #[clap(long)]
    update_exists: bool,
    #[clap(subcommand)]
    subcommand: SubcommandPixivAction,
}

#[derive(Parser)]
enum SubcommandPixivAction {
    Bookmarks(PixivBookmarks),
    Uploads,
}

#[derive(Parser)]
struct PixivBookmarks {
    #[clap(long)]
    private: bool,
}

async fn migrate_guard(db: &Database, fail_if_out_of_date: bool) -> crate::Result<()> {
    if let Some(metadata) = command::migrate::get_metadata(db).await? {
        if fail_if_out_of_date && metadata.version < DB_VERSION {
            return error::MigrationRequired.fail();
        }
        if metadata.version > DB_VERSION {
            return error::DatabaseIsNewer.fail();
        }
    } else {
        db.collection("bowerbird_metadata")
            .insert_one(
                to_bson(&BowerbirdMetadata {
                    version: DB_VERSION,
                })
                .unwrap(),
                None,
            )
            .await
            .context(error::MongoDb)?;
    }
    Ok(())
}

async fn run_internal() -> crate::Result<()> {
    let opts = Main::parse();

    let config_builder = || {
        let config_path = if let Some(c) = &opts.config {
            PathBuf::from(c)
        } else {
            dirs::home_dir().unwrap_or_default().join(".bowerbird")
        }
        .join("config.json");
        let config = config::Config::from_file(&config_path)?;
        debug!("config loaded: {:?}", config_path);

        Ok(config)
    };

    let pre_fn = |fail_if_out_of_date: bool| async move {
        let config = config_builder()?;
        let db_client = mongodb::Client::with_options(
            mongodb::options::ClientOptions::parse(&config.mongodb.uri)
                .await
                .context(error::MongoDb)?,
        )
        .context(error::MongoDb)?;

        debug!("connected to mongodb: {}", config.mongodb.uri);

        let ffmpeg_path = if config.ffmpeg_path.is_empty() {
            PathBuf::from("ffmpeg")
        } else {
            PathBuf::from(&config.ffmpeg_path)
        };

        debug!("checking ffmpeg: {:?}", ffmpeg_path);

        let mut ffmpeg = Command::new(&ffmpeg_path);
        ffmpeg.args(["-hide_banner", "-loglevel", "error"]);
        let ffmpeg_path = match ffmpeg.spawn() {
            Ok(mut child) => {
                let _ = timeout(Duration::from_secs(1), child.wait()).await;
                Some(ffmpeg_path)
            }
            Err(err) => {
                warn!(
                    "ffmpeg not found, some functions will not work: {}: {}",
                    ffmpeg_path.to_string_lossy(),
                    err
                );
                None
            }
        };

        let db = db_client.database(&config.mongodb.database_name);
        migrate_guard(&db, fail_if_out_of_date).await?;

        Ok((config, ffmpeg_path, db))
    };

    match &opts.subcommand {
        SubcommandMain::Migrate => {
            let (_, _, db) = pre_fn(false).await?;
            command::migrate::migrate(&db).await?;
        }
        SubcommandMain::Serve => {
            let (config, _, db) = pre_fn(true).await?;
            crate::server::run(db, config).await?;
        }
        SubcommandMain::Init => {
            config_builder()?;
        }
        SubcommandMain::Pixiv(c) => {
            use pixivcrab::AuthMethod;
            let user_id = c.user_id;
            let limit = c.limit;
            let pre_fn = async {
                let (mut config, ffmpeg_path, db) = pre_fn(true).await?;
                command::pixiv::database::create_indexes(&db).await?;
                let mut api_client = reqwest::ClientBuilder::new();
                if let Some(proxy) = config.pxoxy(&config.pixiv.proxy_api)? {
                    debug!("pixiv api proxy set: {:?}", proxy);
                    api_client = api_client.proxy(proxy);
                }
                if std::env::var("BOWERBIRD_ACCEPT_INVALID_CERTS").is_ok() {
                    warn!("invalid certs will be accepted for pixiv api requests");
                    api_client = api_client.danger_accept_invalid_certs(true);
                }
                let api = pixivcrab::AppApi::new(
                    AuthMethod::RefreshToken(config.pixiv.refresh_token.clone()),
                    &config.pixiv.language,
                    api_client,
                )
                .context(error::PixivApi)?;
                let auth_result = api.auth().await.context(error::PixivApi)?;
                debug!("pixiv authed: {:?}", auth_result);
                info!(
                    "pixiv logged in: {} ({})",
                    auth_result.user.name, auth_result.user.id
                );
                config.pixiv.refresh_token = auth_result.refresh_token;
                config.save()?;
                let selected_user_id = user_id.map_or(auth_result.user.id, |i| i.to_string());
                let downloader =
                    crate::downloader::Aria2Downloader::new(&config.aria2_path).await?;

                let task_config = command::pixiv::TaskConfig {
                    ffmpeg_path,
                    parent_dir: config.sub_dir(&config.pixiv.storage_dir),
                    proxy: config.pxoxy_string(&config.pixiv.proxy_download),
                };
                Ok((db, api, selected_user_id, downloader, task_config))
            };
            match &c.subcommand {
                SubcommandPixiv::Illust(c) => match &c.subcommand {
                    SubcommandPixivAction::Bookmarks(c) => {
                        let (db, api, selected_user_id, downloader, task_config) = pre_fn.await?;
                        command::pixiv::illust_bookmarks(
                            &api,
                            &db,
                            &downloader,
                            &selected_user_id,
                            c.private,
                            limit,
                            &task_config,
                        )
                        .await?;
                        downloader.wait_shutdown().await;
                    }
                    SubcommandPixivAction::Uploads => {
                        let (db, api, selected_user_id, downloader, task_config) = pre_fn.await?;
                        command::pixiv::illust_uploads(
                            &api,
                            &db,
                            &downloader,
                            &selected_user_id,
                            limit,
                            &task_config,
                        )
                        .await?;
                        downloader.wait_shutdown().await;
                    }
                },
                SubcommandPixiv::Novel(c) => {
                    let update_exists = c.update_exists;
                    match &c.subcommand {
                        SubcommandPixivAction::Bookmarks(c) => {
                            let (db, api, selected_user_id, downloader, task_config) =
                                pre_fn.await?;
                            command::pixiv::novel_bookmarks(
                                &api,
                                &db,
                                &downloader,
                                update_exists,
                                &selected_user_id,
                                c.private,
                                limit,
                                &task_config,
                            )
                            .await?;
                        }
                        SubcommandPixivAction::Uploads => {
                            let (db, api, selected_user_id, downloader, task_config) =
                                pre_fn.await?;
                            command::pixiv::novel_uploads(
                                &api,
                                &db,
                                &downloader,
                                update_exists,
                                &selected_user_id,
                                limit,
                                &task_config,
                            )
                            .await?;
                        }
                    };
                }
            }
        }
    };

    Ok(())
}

/// Run the app and return the exit code.
pub async fn run() -> i32 {
    if let Err(e) = run_internal().await {
        error!("{}", e);
        1
    } else {
        0
    }
}
