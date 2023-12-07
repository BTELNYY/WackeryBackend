use clap::Parser;
use indicatif::ParallelProgressIterator;
use lurky::config::LurkyConfig;
use lurky::db;
use lurky::db::{DBPlayer, DB};
use rayon::prelude::*;
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    hash::Hasher,
    path::PathBuf,
    sync::Arc,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
#[derive(Debug, Clone, Parser)]
#[clap(author, version, about, long_about = None)]
struct Args {
    config: PathBuf,
    inputs: Vec<PathBuf>,
}

impl Args {
    fn validate(&self) -> Result<(), String> {
        if !self.config.exists() {
            return Err(format!(
                "Config file does not exist: {}",
                self.config.display()
            ));
        }
        for input in self.inputs.iter() {
            if !input.exists() {
                return Err(format!("Input file does not exist: {}", input.display()));
            }
        }
        if self.inputs.len() == 0 {
            return Err(format!("No inputs provided"));
        }
        Ok(())
    }
}

/*

pub struct DBPlayer {
    pub id: u64,
    pub first_seen: time::DateTime<Utc>,
    pub last_seen: time::DateTime<Utc>,
    #[serde_as(as = "DurationSeconds<i64>")]
    pub play_time: time::Duration,
    pub last_nickname: String,
    pub nicknames: Vec<String>,
    pub flags: Vec<Flag>,
    #[serde_as(as = "DurationSeconds<i64>")]
    pub time_online: time::Duration,
    pub login_amt: u64,
}
 */

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: Deserializer<'de>,
{
    let opt = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}
use serde_with::{serde_as, DurationSeconds};
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
#[allow(non_snake_case)]
struct RawSCP {
    FirstSeen: String, //2022-11-16T23:19:58.0702934Z,
    LastSeen: String, //"2022-11-16T23:58:44.5669335Z","PlayTime":0,"SteamID":"76561197960804536@steam","LastNickname":"UnholyChalupa","Usernames":{},"PFlags":[],"TimeOnline":0,"LoginAmount":0}
    #[serde_as(as = "DurationSeconds<i64>")]
    PlayTime: time::Duration,
    SteamID: String,
    LastNickname: Option<String>,
    #[serde(deserialize_with = "deserialize_null_default")]
    Usernames: HashMap<String, String>, // first type doesnt really matter
    PFlags: Vec<RawFlag>, // ill deal with you later...
    #[serde_as(as = "DurationSeconds<i64>")]
    TimeOnline: time::Duration,
    LoginAmount: u64,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
#[allow(non_snake_case)]
struct RawFlag {
    Comment: String,
    Flag: u64,
    Issuer: String,
    #[serde(with = "time::serde::rfc3339")]
    IssueTime: OffsetDateTime, // istg
}

impl RawFlag {
    fn to_flag(self) -> db::Flag {
        db::Flag {
            comment: self.Comment,
            flag: self.Flag as i64,
            issuer: self.Issuer,
            issued_at: self.IssueTime,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let args = Args::parse();
    if let Err(e) = args.validate() {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
    let config = Arc::new(LurkyConfig::parse_data(std::fs::File::open(args.config)?));
    println!("{:?}", config);
    let mut db = db::create_db_from_config(&config)?;
    db.setup().await?;
    let db = Arc::new(db);
    println!("Collecting files...");
    let inputs: HashSet<String> = args
        .inputs
        .iter()
        .flat_map(|input| {
            let input = fs::read_dir(input)
                .expect("Failed to read input directory, good ol time of use, time of check bug.");
            input
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    e.path()
                        .file_name()
                        .and_then(|e| Some(e.to_string_lossy().to_string()))
                })
                .filter(|e| e.ends_with(".json"))
        })
        .collect();
    println!("Found {} users", inputs.len());
    // collect files
    println!("Collecting unique users...");
    let files: Vec<Vec<PathBuf>> = inputs
        .iter()
        .map(|i| {
            args.inputs
                .iter()
                .filter_map(|f| {
                    let path = f.join(i);
                    if path.exists() {
                        Some(path)
                    } else {
                        None
                    }
                })
                .collect()
        })
        .collect();
    println!("Begining migration...");
    let pb = indicatif::ProgressBar::new(files.len() as u64);
    pb.set_style(
        indicatif::ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} ({eta})",
            )
            .unwrap()
            .progress_chars("#>-"),
    );
    // here we go
    let async_handle = Arc::new(tokio::runtime::Handle::current());
    files.par_iter().progress_with(pb).for_each(|migrator| {
        //println!("Gaming.");
        let db = db.clone();
        let async_handle = async_handle.clone();
        //println!("Migrating...");
        let read_files = migrator
            .iter()
            .map(|f| {
                let file = fs::File::open(f)
                    .expect(format!("Failed to open file {}", f.display()).as_str());
                let buferr = std::io::BufReader::new(file);
                serde_json::from_reader(buferr)
                    .expect(format!("Failed to read file {}", f.display()).as_str())
            })
            .collect::<Vec<RawSCP>>();
        // time to merge them
        // first, the start and end date

        let last_seen = read_files
            .iter()
            .map(|e| {
                time::OffsetDateTime::parse(&e.LastSeen, &Rfc3339)
                    .expect("Failed to parse last seen")
            })
            .max()
            .expect("Failed to find max date");
        let first_seen = read_files
            .iter()
            .map(|e| {
                time::OffsetDateTime::parse(&e.FirstSeen, &Rfc3339)
                    .unwrap_or_else(|_| last_seen.clone())
            })
            .min()
            .expect("Failed to find min date");
        // playtime is easy
        let play_time: time::Duration = read_files
            .iter()
            .map(|e| e.PlayTime)
            .reduce(|acc, e| acc + e)
            .expect("Failed to find playtime");
        // steam id is easy
        let steam_id = read_files[0].SteamID.clone();

        // last nickname should be based on last seen
        let last_nickname = read_files
            .iter()
            .find(|e| {
                time::OffsetDateTime::parse(&e.LastSeen, &Rfc3339)
                    .expect("Failed to parse last seen")
                    == last_seen
            })
            .expect("Failed to find last nickname")
            .LastNickname
            .clone()
            .unwrap_or_else(|| {
                // northwood, parse from filename
                let filename = migrator[0]
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                let last_nick = filename.split('@').next().unwrap();
                last_nick.to_string()
            });
        //println!("Last nickname: {}", last_nickname);

        let all_nicks: HashSet<String> = read_files
            .iter()
            .flat_map(|e| {
                (e.Usernames.iter().map(|(_, v)| v.clone()))
                    .chain(e.LastNickname.clone().into_iter())
            })
            .collect();
        //println!("All nicks: {:?}", all_nicks);

        // simply merge the flags array
        let flags: Vec<&RawFlag> = read_files.iter().flat_map(|e| e.PFlags.iter()).collect();

        // time online is easy
        let time_online: time::Duration = read_files
            .iter()
            .map(|e| e.TimeOnline)
            .reduce(|acc, e| acc + e)
            .expect("Failed to find time online");

        // finally, login amount
        let login_amount: u64 = read_files.iter().map(|e| e.LoginAmount).sum();

        let id = steam_id.split("@").next().unwrap();
        let id = if steam_id.ends_with("northwood") {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            hasher.write(id.as_bytes());
            hasher.finish()
        } else {
            id.parse::<u64>().expect("Unable to parse id")
        };

        // create a player
        let player = DBPlayer {
            id,
            first_seen,
            last_seen,
            play_time,
            last_nickname,
            nicknames: all_nicks.into_iter().collect(),
            flags: flags.into_iter().map(|e| e.clone().to_flag()).collect(),
            time_online,
            login_amt: login_amount,
        };

        // do the thing here
        async_handle.block_on(update_or_add_player(db, player));
    });
    //pb.finish();
    Ok(())
}

async fn update_or_add_player(db: Arc<Box<dyn DB>>, player: DBPlayer) {
    if db.has_player(player.id).await.unwrap() {
        db.update_player(player).await.unwrap();
    } else {
        db.create_player(player).await.unwrap();
    }
}
