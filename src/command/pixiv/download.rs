use aria2_ws::TaskOptions;
use futures::FutureExt;
use lazy_static::lazy_static;
use regex::Regex;

use std::{collections::HashMap, path::PathBuf};
use tokio::task::spawn_blocking;

use crate::{
    downloader::{Aria2Downloader, Task, TaskHooks},
    error::{self, BoxError},
    log::warning,
};

use mongodb::{
    bson::{doc, Document},
    Collection,
};

use path_slash::PathBufExt;

use super::{utils, TaskConfig};
lazy_static! {
    /// Match the pximg URL.
    ///
    /// # Example
    ///
    /// Matching the URL
    /// `https://i.pximg.net/img-original/img/2021/08/22/22/03/33/92187206_p0.jpg`
    ///
    /// Groups:
    ///
    /// __0__ `/2021/08/22/22/03/33/92187206_p0.jpg`
    ///
    /// __1__ `2021/08/22/22/03/33`
    ///
    /// __2__ `92187206_p0.jpg`
    ///
    /// __3__ `92187206_p0`
    ///
    /// __4__ `jpg`
    static ref RE_ILLUST_URL: Regex =
        Regex::new(r"/(\d{4}/\d{2}/\d{2}/\d{2}/\d{2}/\d{2})/((.*)\.(.*))$").unwrap();
}

macro_rules! try_skip {
    ($res:expr) => {
        match $res {
            Ok(val) => val,
            Err(e) => {
                warning!("{}", e);
                continue;
            }
        }
    };
}

async fn on_success_ugoira(
    zip_url: String,
    zip_path: PathBuf,
    c_image: Collection<Document>,
    path_slash: String,
    ugoira_frame_delay: Vec<i32>,
    ffmpeg_path: Option<PathBuf>,
) -> Result<(), BoxError> {
    let with_mp4 = ffmpeg_path.is_some();
    if let Some(ffmpeg_path) = ffmpeg_path {
        let zip_path = zip_path.clone();
        spawn_blocking(move || utils::ugoira_to_mp4(&ffmpeg_path, &zip_path, ugoira_frame_delay))
            .await
            .unwrap()?;
    }
    let zip_size: i64 = tokio::fs::metadata(&zip_path).await?.len().try_into()?;

    super::database::save_image_ugoira(&c_image, zip_url, zip_path, path_slash, zip_size, with_mp4)
        .await?;

    Ok(())
}

async fn on_success_illust(
    url: String,
    image_path: PathBuf,
    c_image: Collection<Document>,
    path_slash: String,
) -> Result<(), BoxError> {
    let size: i64 = tokio::fs::metadata(&image_path).await?.len().try_into()?;
    let ((w, h), rgb_v) = {
        let image_path = image_path.clone();
        spawn_blocking(move || utils::get_palette(&image_path))
    }
    .await
    .unwrap()?;
    super::database::save_image(&c_image, size, (w, h), rgb_v, url, path_slash, image_path).await?;

    Ok(())
}

fn task_from_illust(
    c_image: Collection<Document>,
    url: Option<String>,
    user_id: &str,
    illust_id: &str,
    is_multi_page: bool,
    ugoira_frame_delay: Option<Vec<i32>>,
    task_config: &TaskConfig,
) -> crate::Result<Option<Task>> {
    let url = url.ok_or(
        error::PixivParse {
            message: "empty url".to_string(),
        }
        .build(),
    )?;

    let captures = RE_ILLUST_URL.captures(&url).ok_or(
        error::PixivParse {
            message: format!("cannot match url with RE_ILLUST_URL: {}", url),
        }
        .build(),
    )?;
    let date = captures.get(1).unwrap().as_str().replace("/", "");

    let path_slash = if is_multi_page {
        format!(
            "{}/{}_{}/{}",
            user_id,
            illust_id,
            date,
            captures.get(2).unwrap().as_str()
        )
    } else {
        format!(
            "{}/{}_{}.{}",
            user_id,
            captures.get(3).unwrap().as_str(), // filename with page id
            date,
            captures.get(4).unwrap().as_str(), // extension
        )
    };

    let path = task_config
        .parent_dir
        .join(PathBuf::from_slash(&path_slash));

    if path.exists() {
        return Ok(None);
    }

    let on_success_hook = if let Some(ugoira_frame_delay) = ugoira_frame_delay {
        // The task is an ugoira zip.
        on_success_ugoira(
            url.clone(),
            path.clone(),
            c_image,
            path_slash,
            ugoira_frame_delay,
            task_config.ffmpeg_path.clone(),
        )
        .boxed()
    } else {
        on_success_illust(url.clone(), path.clone(), c_image, path_slash).boxed()
    };

    Ok(Some(Task {
        hooks: Some(TaskHooks {
            on_success: Some(on_success_hook),
            ..Default::default()
        }),
        options: Some(TaskOptions {
            header: Some(vec!["Referer: https://app-api.pixiv.net/".to_string()]),
            all_proxy: task_config.proxy.clone(),
            out: Some(path.to_string_lossy().to_string()),
            ..Default::default()
        }),
        url,
    }))
}

pub async fn download_illusts(
    illusts: &Vec<pixivcrab::models::illust::Illust>,
    ugoira_map: &mut HashMap<String, (String, Vec<i32>)>,
    downloader: &Aria2Downloader,
    c_image: &Collection<Document>,
    items_sent: &mut u32,
    limit: Option<u32>,
    task_config: &TaskConfig,
) -> crate::Result<()> {
    let mut tasks = Vec::new();
    for i in illusts {
        if super::limit_reached(limit, *items_sent) {
            break;
        }
        *items_sent += 1;

        if !i.visible {
            continue;
        }
        let illust_id = i.id.to_string();
        let is_ugoira = i.r#type == "ugoira";

        if is_ugoira {
            if let Some((zip_url, delay)) = ugoira_map.remove(&illust_id) {
                let zip_url = zip_url.replace("600x600", "1920x1080");
                match task_from_illust(
                    c_image.clone(),
                    // get higher resolution images
                    Some(zip_url.clone()),
                    &i.user.id.to_string(),
                    &illust_id,
                    true,
                    Some(delay),
                    task_config,
                ) {
                    Ok(task) => {
                        if let Some(task) = task {
                            tasks.push(task);
                        }
                    }
                    Err(err) => {
                        warning!("Fail to build task from {}: {}", zip_url, err)
                    }
                }
            }
        }

        if i.page_count == 1 {
            if let Some(task) = try_skip!(task_from_illust(
                c_image.clone(),
                i.meta_single_page.original_image_url.clone(),
                &i.user.id.to_string(),
                &illust_id,
                is_ugoira,
                None,
                task_config
            )) {
                tasks.push(task);
            }
        } else {
            for img in &i.meta_pages {
                if let Some(task) = try_skip!(task_from_illust(
                    c_image.clone(),
                    img.image_urls.original.clone(),
                    &i.user.id.to_string(),
                    &illust_id,
                    true,
                    None,
                    task_config
                )) {
                    tasks.push(task);
                }
            }
        }
    }
    downloader.add_tasks(tasks).await?;
    Ok(())
}