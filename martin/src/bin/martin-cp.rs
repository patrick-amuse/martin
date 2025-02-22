use std::f64::consts::PI;
use std::fmt::{Debug, Display, Formatter};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use actix_http::error::ParseError;
use actix_http::test::TestRequest;
use actix_web::http::header::{AcceptEncoding, Header as _, ACCEPT_ENCODING};
use clap::Parser;
use futures::stream::{self, StreamExt};
use futures::TryStreamExt;
use log::{debug, error, info, log_enabled};
use martin::args::{Args, ExtraArgs, MetaArgs, OsEnv, PgArgs, SrvArgs};
use martin::srv::{get_tile_content, merge_tilejson, RESERVED_KEYWORDS};
use martin::{
    append_rect, read_config, Config, IdResolver, MartinError, MartinResult, ServerState, Source,
    TileCoord, TileData, TileRect,
};
use martin_tile_utils::TileInfo;
use mbtiles::sqlx::SqliteConnection;
use mbtiles::{
    init_mbtiles_schema, is_empty_database, CopyDuplicateMode, MbtType, MbtTypeCli, Mbtiles,
};
use tilejson::Bounds;
use tokio::sync::mpsc::channel;
use tokio::time::Instant;
use tokio::try_join;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const SAVE_EVERY: Duration = Duration::from_secs(60);
const PROGRESS_REPORT_AFTER: u64 = 100;
const PROGRESS_REPORT_EVERY: Duration = Duration::from_secs(2);
const BATCH_SIZE: usize = 1000;

#[derive(Parser, Debug, PartialEq, Default)]
#[command(
    about = "A tool to bulk copy tiles from any Martin-supported sources into an mbtiles file",
    version
)]
pub struct CopierArgs {
    #[command(flatten)]
    pub copy: CopyArgs,
    #[command(flatten)]
    pub meta: MetaArgs,
    #[command(flatten)]
    pub pg: Option<PgArgs>,
}

#[serde_with::serde_as]
#[derive(clap::Args, Debug, PartialEq, Default, serde::Deserialize, serde::Serialize)]
pub struct CopyArgs {
    /// Name of the source to copy from.
    #[arg(short, long)]
    pub source: String,
    /// Path to the mbtiles file to copy to.
    #[arg(short, long)]
    pub output_file: PathBuf,
    /// Output format of the new destination file. Ignored if the file exists. Defaults to 'normalized'.
    #[arg(
        long = "mbtiles-type",
        alias = "dst-type",
        value_name = "SCHEMA",
        value_enum
    )]
    pub mbt_type: Option<MbtTypeCli>,
    /// Optional query parameter (in URL query format) for the sources that support it (e.g. Postgres functions)
    #[arg(long)]
    pub url_query: Option<String>,
    /// Optional accepted encoding parameter as if the browser sent it in the HTTP request.
    /// If set to multiple values like `gzip,br`, martin-cp will use the first encoding,
    /// or re-encode if the tile is already encoded and that encoding is not listed.  
    /// Use `identity` to disable compression. Ignored for non-encodable tiles like PNG and JPEG.
    #[arg(long, alias = "encodings", default_value = "gzip")]
    pub encoding: String,
    /// Specify the behaviour when generated tile already exists in the destination file.
    #[arg(long, value_enum, default_value_t = CopyDuplicateMode::default())]
    pub on_duplicate: CopyDuplicateMode,
    /// Number of concurrent connections to use.
    #[arg(long, default_value = "1")]
    pub concurrency: Option<usize>,
    /// Bounds to copy. Can be specified multiple times. Overlapping regions will be handled correctly.
    #[arg(long)]
    pub bbox: Vec<Bounds>,
    /// Minimum zoom level to copy
    #[arg(long, alias = "minzoom", conflicts_with("zoom_levels"))]
    pub min_zoom: Option<u8>,
    /// Maximum zoom level to copy
    #[arg(
        long,
        alias = "maxzoom",
        conflicts_with("zoom_levels"),
        required_unless_present("zoom_levels")
    )]
    pub max_zoom: Option<u8>,
    /// List of zoom levels to copy
    #[arg(short, long, alias = "zooms", value_delimiter = ',')]
    pub zoom_levels: Vec<u8>,
    /// Skip generating a global hash for mbtiles validation. By default, `martin-cp` will compute and update `agg_tiles_hash` metadata value.
    #[arg(long)]
    pub skip_agg_tiles_hash: bool,
    /// Set additional metadata values. Must be set as "key=value" pairs. Can be specified multiple times.
    #[arg(long, value_name="KEY=VALUE", value_parser = parse_key_value)]
    pub set_meta: Vec<(String, String)>,
}

fn parse_key_value(s: &str) -> Result<(String, String), String> {
    let mut parts = s.splitn(2, '=');
    let key = parts.next().unwrap();
    let value = parts
        .next()
        .ok_or_else(|| format!("Invalid key=value pair: {s}"))?;
    if key.is_empty() || value.is_empty() {
        Err(format!("Invalid key=value pair: {s}"))
    } else {
        Ok((key.to_string(), value.to_string()))
    }
}

async fn start(copy_args: CopierArgs) -> MartinCpResult<()> {
    info!("Martin-CP tile copier v{VERSION}");

    let env = OsEnv::default();
    let save_config = copy_args.meta.save_config.clone();
    let mut config = if let Some(ref cfg_filename) = copy_args.meta.config {
        info!("Using {}", cfg_filename.display());
        read_config(cfg_filename, &env)?
    } else {
        info!("Config file is not specified, auto-detecting sources");
        Config::default()
    };

    let args = Args {
        meta: copy_args.meta,
        extras: ExtraArgs::default(),
        srv: SrvArgs::default(),
        pg: copy_args.pg,
    };

    args.merge_into_config(&mut config, &env)?;
    config.finalize()?;
    let sources = config.resolve(IdResolver::new(RESERVED_KEYWORDS)).await?;

    if let Some(file_name) = save_config {
        config.save_to_file(file_name)?;
    } else {
        info!("Use --save-config to save or print configuration.");
    }

    run_tile_copy(copy_args.copy, sources).await
}

/// Convert longitude and latitude to tile index
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn tile_index(lon: f64, lat: f64, zoom: u8) -> (u32, u32) {
    let n = f64::from(1_u32 << zoom);
    let x = ((lon + 180.0) / 360.0 * n).floor() as u32;
    let y = ((1.0 - (lat.to_radians().tan() + 1.0 / lat.to_radians().cos()).ln() / PI) / 2.0 * n)
        .floor() as u32;
    let max_value = (1_u32 << zoom) - 1;
    (x.min(max_value), y.min(max_value))
}

fn compute_tile_ranges(args: &CopyArgs) -> Vec<TileRect> {
    let mut ranges = Vec::new();
    let mut zooms_vec = Vec::new();
    let zooms = if let Some(max_zoom) = args.max_zoom {
        let min_zoom = args.min_zoom.unwrap_or(0);
        zooms_vec.extend(min_zoom..=max_zoom);
        &zooms_vec
    } else {
        &args.zoom_levels
    };
    let boxes = if args.bbox.is_empty() {
        vec![Bounds::MAX_TILED]
    } else {
        args.bbox.clone()
    };
    for zoom in zooms {
        for bbox in &boxes {
            let (min_x, min_y) = tile_index(bbox.left, bbox.top, *zoom);
            let (max_x, max_y) = tile_index(bbox.right, bbox.bottom, *zoom);
            append_rect(
                &mut ranges,
                TileRect::new(*zoom, min_x, min_y, max_x, max_y),
            );
        }
    }
    ranges
}

struct TileXyz {
    xyz: TileCoord,
    data: TileData,
}

impl Debug for TileXyz {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} - {} bytes", self.xyz, self.data.len())
    }
}

struct Progress {
    // needed to compute elapsed time
    start_time: Instant,
    total: u64,
    empty: AtomicU64,
    non_empty: AtomicU64,
}

impl Progress {
    pub fn new(tiles: &[TileRect]) -> Self {
        let total = tiles.iter().map(TileRect::size).sum();
        Progress {
            start_time: Instant::now(),
            total,
            empty: AtomicU64::default(),
            non_empty: AtomicU64::default(),
        }
    }
}

type MartinCpResult<T> = Result<T, MartinCpError>;

#[derive(Debug, thiserror::Error)]
enum MartinCpError {
    #[error(transparent)]
    Martin(#[from] MartinError),
    #[error("Unable to parse encodings argument: {0}")]
    EncodingParse(#[from] ParseError),
    #[error(transparent)]
    Actix(#[from] actix_web::Error),
    #[error(transparent)]
    Mbt(#[from] mbtiles::MbtError),
}

impl Display for Progress {
    #[allow(clippy::cast_precision_loss)]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let elapsed = self.start_time.elapsed();
        let elapsed_s = elapsed.as_secs_f32();
        let non_empty = self.non_empty.load(Ordering::Relaxed);
        let empty = self.empty.load(Ordering::Relaxed);
        let done = non_empty + empty;
        let percent = done * 100 / self.total;
        let speed = if elapsed_s > 0.0 {
            done as f32 / elapsed_s
        } else {
            0.0
        };
        write!(
            f,
            "[{elapsed:.1?}] {percent:.2}% @ {speed:.1}/s | ✓ {non_empty} □ {empty}"
        )?;

        let left = self.total - done;
        if left == 0 {
            write!(f, " | done")
        } else if done == 0 {
            write!(f, " | ??? left")
        } else {
            let left = Duration::from_secs_f32(elapsed_s * left as f32 / done as f32);
            write!(f, " | {left:.0?} left")
        }
    }
}

/// Given a list of tile ranges, iterate over all tiles in the ranges
fn iterate_tiles(tiles: Vec<TileRect>) -> impl Iterator<Item = TileCoord> {
    tiles.into_iter().flat_map(|t| {
        let z = t.zoom;
        (t.min_x..=t.max_x)
            .flat_map(move |x| (t.min_y..=t.max_y).map(move |y| TileCoord { z, x, y }))
    })
}

async fn run_tile_copy(args: CopyArgs, state: ServerState) -> MartinCpResult<()> {
    let output_file = &args.output_file;
    let concurrency = args.concurrency.unwrap_or(1);
    let (sources, _use_url_query, info) = state.tiles.get_sources(args.source.as_str(), None)?;
    let sources = sources.as_slice();
    let tile_info = sources.first().unwrap().get_tile_info();
    let (tx, mut rx) = channel::<TileXyz>(500);
    let tiles = compute_tile_ranges(&args);
    let mbt = Mbtiles::new(output_file)?;
    let mut conn = mbt.open_or_new().await?;
    let mbt_type = init_schema(&mbt, &mut conn, sources, tile_info, args.mbt_type).await?;
    let query = args.url_query.as_deref();
    let req = TestRequest::default()
        .insert_header((ACCEPT_ENCODING, args.encoding.as_str()))
        .finish();
    let accept_encoding = AcceptEncoding::parse(&req)?;
    let encodings = Some(&accept_encoding);

    let progress = Progress::new(&tiles);
    info!(
        "Copying {} {tile_info} tiles from {} to {}",
        progress.total,
        args.source,
        args.output_file.display()
    );

    try_join!(
        async move {
            stream::iter(iterate_tiles(tiles))
                .map(MartinResult::Ok)
                .try_for_each_concurrent(concurrency, |xyz| {
                    let tx = tx.clone();
                    async move {
                        let tile = get_tile_content(sources, info, &xyz, query, encodings).await?;
                        let data = tile.data;
                        tx.send(TileXyz { xyz, data })
                            .await
                            .map_err(|e| MartinError::InternalError(e.into()))?;
                        Ok(())
                    }
                })
                .await
        },
        async {
            let mut last_saved = Instant::now();
            let mut last_reported = Instant::now();
            let mut batch = Vec::with_capacity(BATCH_SIZE);
            while let Some(tile) = rx.recv().await {
                debug!("Generated tile {tile:?}");
                let done = if tile.data.is_empty() {
                    progress.empty.fetch_add(1, Ordering::Relaxed)
                } else {
                    batch.push((tile.xyz.z, tile.xyz.x, tile.xyz.y, tile.data));
                    if batch.len() >= BATCH_SIZE || last_saved.elapsed() > SAVE_EVERY {
                        mbt.insert_tiles(&mut conn, mbt_type, args.on_duplicate, &batch)
                            .await?;
                        batch.clear();
                        last_saved = Instant::now();
                    }
                    progress.non_empty.fetch_add(1, Ordering::Relaxed)
                };
                if done % PROGRESS_REPORT_AFTER == (PROGRESS_REPORT_AFTER - 1)
                    && last_reported.elapsed() > PROGRESS_REPORT_EVERY
                {
                    info!("{progress}");
                    last_reported = Instant::now();
                }
            }
            if !batch.is_empty() {
                mbt.insert_tiles(&mut conn, mbt_type, args.on_duplicate, &batch)
                    .await?;
            }
            Ok(())
        }
    )?;

    info!("{progress}");

    for (key, value) in args.set_meta {
        info!("Setting metadata key={key} value={value}");
        mbt.set_metadata_value(&mut conn, &key, value).await?;
    }

    if !args.skip_agg_tiles_hash {
        if progress.non_empty.load(Ordering::Relaxed) == 0 {
            info!("No tiles were copied, skipping agg_tiles_hash computation");
        } else {
            info!("Computing agg_tiles_hash value...");
            mbt.update_agg_tiles_hash(&mut conn).await?;
        }
    }

    Ok(())
}

async fn init_schema(
    mbt: &Mbtiles,
    conn: &mut SqliteConnection,
    sources: &[&dyn Source],
    tile_info: TileInfo,
    mbt_type: Option<MbtTypeCli>,
) -> Result<MbtType, MartinError> {
    Ok(if is_empty_database(&mut *conn).await? {
        let mbt_type = match mbt_type.unwrap_or(MbtTypeCli::Normalized) {
            MbtTypeCli::Flat => MbtType::Flat,
            MbtTypeCli::FlatWithHash => MbtType::FlatWithHash,
            MbtTypeCli::Normalized => MbtType::Normalized { hash_view: true },
        };
        init_mbtiles_schema(&mut *conn, mbt_type).await?;
        let mut tj = merge_tilejson(sources, String::new());
        tj.other.insert(
            "format".to_string(),
            serde_json::Value::String(tile_info.format.to_string()),
        );
        tj.other.insert(
            "generator".to_string(),
            serde_json::Value::String(format!("martin-cp v{VERSION}")),
        );
        mbt.insert_metadata(&mut *conn, &tj).await?;
        mbt_type
    } else {
        mbt.detect_type(&mut *conn).await?
    })
}

#[actix_web::main]
async fn main() {
    let env = env_logger::Env::default().default_filter_or("martin_cp=info");
    env_logger::Builder::from_env(env).init();

    start(CopierArgs::parse())
        .await
        .unwrap_or_else(|e| on_error(e));
}

fn on_error<E: Display>(e: E) -> ! {
    // Ensure the message is printed, even if the logging is disabled
    if log_enabled!(log::Level::Error) {
        error!("{e}");
    } else {
        eprintln!("{e}");
    }
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use insta::assert_yaml_snapshot;

    use super::*;

    #[test]
    fn test_tile_index() {
        assert_eq!((0, 0), tile_index(-180.0, 85.0511, 0));
    }

    #[test]
    fn test_compute_tile_ranges() {
        let world = Bounds::MAX_TILED;
        let bbox_ca = Bounds::from_str("-124.482,32.5288,-114.1307,42.0095").unwrap();
        let bbox_ca_south = Bounds::from_str("-118.6681,32.5288,-114.1307,34.8233").unwrap();
        let bbox_mi = Bounds::from_str("-86.6271,41.6811,-82.3095,45.8058").unwrap();
        let bbox_usa = Bounds::from_str("-124.8489,24.3963,-66.8854,49.3843").unwrap();

        assert_yaml_snapshot!(compute_tile_ranges(&args(&[world], &[0])), @r###"
        ---
        - "0: (0,0) - (0,0)"
        "###);

        assert_yaml_snapshot!(compute_tile_ranges(&args(&[world], &[3,7])), @r###"
        ---
        - "3: (0,0) - (7,7)"
        - "7: (0,0) - (127,127)"
        "###);

        assert_yaml_snapshot!(compute_tile_ranges(&arg_minmax(&[world], 2, 4)), @r###"
        ---
        - "2: (0,0) - (3,3)"
        - "3: (0,0) - (7,7)"
        - "4: (0,0) - (15,15)"
        "###);

        assert_yaml_snapshot!(compute_tile_ranges(&args(&[world], &[14])), @r###"
        ---
        - "14: (0,0) - (16383,16383)"
        "###);

        assert_yaml_snapshot!(compute_tile_ranges(&args(&[bbox_usa], &[14])), @r###"
        ---
        - "14: (2509,5599) - (5147,7046)"
        "###);

        assert_yaml_snapshot!(compute_tile_ranges(&args(&[bbox_usa, bbox_mi, bbox_ca], &[14])), @r###"
        ---
        - "14: (2509,5599) - (5147,7046)"
        "###);

        assert_yaml_snapshot!(compute_tile_ranges(&args(&[bbox_ca_south, bbox_mi, bbox_ca], &[14])), @r###"
        ---
        - "14: (2791,6499) - (2997,6624)"
        - "14: (4249,5841) - (4446,6101)"
        - "14: (2526,6081) - (2790,6624)"
        - "14: (2791,6081) - (2997,6498)"
        "###);
    }

    fn args(bbox: &[Bounds], zooms: &[u8]) -> CopyArgs {
        CopyArgs {
            bbox: bbox.to_vec(),
            zoom_levels: zooms.to_vec(),
            ..Default::default()
        }
    }

    fn arg_minmax(bbox: &[Bounds], min_zoom: u8, max_zoom: u8) -> CopyArgs {
        CopyArgs {
            bbox: bbox.to_vec(),
            min_zoom: Some(min_zoom),
            max_zoom: Some(max_zoom),
            ..Default::default()
        }
    }
}
