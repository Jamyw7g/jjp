use std::{borrow::Cow, iter::repeat, time::Duration};

use reqwest::{
    header::{HeaderValue, CONTENT_RANGE, RANGE},
    Client, IntoUrl,
};

pub async fn prepare<U: IntoUrl>(client: Client, url: U) -> anyhow::Result<(bool, usize)> {
    let resp = client
        .get(url)
        .header(RANGE, HeaderValue::from_static("bytes=0-0"))
        .send()
        .await?;

    if resp.status().as_u16() / 100 == 2 {
        let length: usize = resp
            .headers()
            .get(CONTENT_RANGE)
            .unwrap()
            .to_str()?
            .rsplit_once('/')
            .unwrap()
            .1
            .parse()?;
        Ok((true, length))
    } else {
        Ok((false, 0))
    }
}

const NAME_LEN: usize = 19;
const BAR_LEN: usize = 28;
const UNITS: [&str; 4] = ["B ", "KB", "MB", "GB"];
const ELLIPSIS: &str = "...";

pub fn show_bar(
    name: &str,
    received: usize,
    total: usize,
    bytes: usize,
    dur: Duration,
    con: usize,
) -> String {
    let name_chars = name.chars().collect::<Vec<_>>();
    let mut name = Cow::from(name);
    if name_chars.len() > NAME_LEN {
        let mut dst_name = String::with_capacity(NAME_LEN);
        let len = (NAME_LEN - ELLIPSIS.len()) / 2;
        for i in 0..len {
            dst_name.push(name_chars[i]);
        }
        dst_name.push_str(ELLIPSIS);
        for i in name_chars.len() - len..name_chars.len() {
            dst_name.push(name_chars[i]);
        }
        name = Cow::from(dst_name);
    }
    let dur = dur.as_secs_f32();
    let speed = (bytes as f32 / dur) as usize;
    let percent = if total != 0 {
        (100.0 * received as f32) / (total as f32)
    } else {
        0.0
    };
    let mut bar = String::with_capacity(BAR_LEN);
    bar.push('[');
    let head = ((BAR_LEN - 2) as f32 * percent / 100.0).ceil() as usize;
    let tail = BAR_LEN - 2 - head;
    repeat('>').take(head).for_each(|ch| bar.push(ch));
    repeat('_').take(tail).for_each(|ch| bar.push(ch));
    bar.push(']');

    format!(
        "{} {} {:02.02}% {}/{} {}/s con:{}",
        name,
        bar,
        percent,
        to_unit(received),
        to_unit(total),
        to_unit(speed),
        con
    )
}

fn to_unit(num: usize) -> String {
    let mut num = num as f32;
    let mut idx = 0;
    while num > 99.9 && idx + 1 < UNITS.len() {
        num /= 1024.0;
        idx += 1;
    }

    format!("{:02.02}{}", num, UNITS[idx])
}
