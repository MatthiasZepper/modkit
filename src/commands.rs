use std::collections::{HashMap, HashSet};
use std::io::BufWriter;
use std::num::ParseFloatError;
use std::path::PathBuf;
use std::thread;

use anyhow::{Context, Result as AnyhowResult};
use clap::{Args, Subcommand};
use crossbeam_channel::bounded;
use indicatif::{
    MultiProgress, ParallelProgressIterator, ProgressBar, ProgressStyle,
};
use log::{debug, info};
use rayon::prelude::*;
use rust_htslib::bam;
use rust_htslib::bam::record::{Aux, AuxArray};
use rust_htslib::bam::Read;

use crate::errs::{InputError, RunError};
use crate::interval_chunks::IntervalChunks;
use crate::logging::init_logging;
use crate::mod_bam::{
    base_mod_probs_from_record, collapse_mod_probs, format_mm_ml_tag,
    CollapseMethod, DeltaListConverter,
};
use crate::mod_base_code::{DnaBase, ModCode};
use crate::mod_pileup::{process_region, ModBasePileup, PileupNumericOptions};
use crate::motif_bed::motif_bed;
use crate::summarize::summarize_modbam;
use crate::thresholds::{
    calc_threshold_from_bam, sample_modbase_probs, Percentiles,
};
use crate::util::record_is_secondary;
use crate::writers::{BEDWriter, BedMethylWriter, OutWriter, TsvWriter};

#[derive(Subcommand)]
pub enum Commands {
    /// Collapse N-way base modification calls to (N-1)-way
    AdjustMods(Adjust),
    /// Pileup (combine) mod calls across genomic positions.
    Pileup(ModBamPileup),
    /// Get an estimate of the distribution of mod-base prediction probabilities
    SampleProbs(SampleModBaseProbs),
    /// Summarize the mod tags present in a BAM and get basic statistics
    Summary(ModSummarize),
    /// Create BED file with all locations of a motif
    MotifBed(MotifBed),
}

impl Commands {
    pub fn run(&self) -> Result<(), String> {
        match self {
            Self::AdjustMods(x) => x.run(),
            Self::Pileup(x) => x.run(),
            Self::SampleProbs(x) => x.run(),
            Self::Summary(x) => x.run(),
            Self::MotifBed(x) => x.run(),
        }
    }
}

fn check_collapse_method(raw_method: &str) -> Result<String, String> {
    match raw_method {
        "norm" => Ok(raw_method.to_owned()),
        "dist" => Ok(raw_method.to_owned()),
        _ => Err(format!("unknown method {raw_method}")),
    }
}

#[derive(Args)]
pub struct Adjust {
    /// BAM file to collapse mod call from
    in_bam: PathBuf,
    /// File path to new BAM file
    out_bam: PathBuf,
    // /// Canonical base to flatten calls for
    // #[arg(
    //     short = 'b',
    //     long,
    //     default_value_t = 'C',
    //     conflicts_with = "convert"
    // )]
    // canonical_base: char,
    /// mod base code to flatten/remove
    #[arg(
        long,
        conflicts_with = "convert",
        required_unless_present = "convert"
    )]
    ignore: Option<char>,
    /// number of threads to use
    #[arg(short, long, default_value_t = 4)]
    threads: usize,
    /// number of threads to use
    #[arg(short, long = "ff", default_value_t = false)]
    fail_fast: bool,
    /// Method to use to collapse mod calls, 'norm', 'dist'.
    #[arg(
        long,
        default_value_t = String::from("norm"),
        value_parser = check_collapse_method,
        requires = "ignore",
    )]
    method: String,
    /// Convert one mod-tag to another, summing the probabilities together if
    /// the retained mod tag is already present.
    #[arg(group = "prob_args", long, action = clap::ArgAction::Append, num_args = 2)]
    convert: Option<Vec<char>>,

    /// Output debug logs to file at this path
    #[arg(long)]
    log_filepath: Option<PathBuf>,
}

pub(crate) fn get_spinner() -> ProgressBar {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template(
            "{spinner:.blue} [{elapsed_precise}] {pos} {msg}",
        )
        .unwrap()
        .tick_strings(&[
            "▹▹▹▹▹",
            "▸▹▹▹▹",
            "▹▸▹▹▹",
            "▹▹▸▹▹",
            "▹▹▹▸▹",
            "▹▹▹▹▸",
            "▪▪▪▪▪",
        ]),
    );
    spinner
}

type CliResult<T> = Result<T, RunError>;

fn adjust_mod_probs(
    mut record: bam::Record,
    methods: &[(DnaBase, CollapseMethod)],
) -> CliResult<bam::Record> {
    if record_is_secondary(&record) {
        return Err(RunError::new_skipped("not primary"));
    }
    if record.seq_len() == 0 {
        return Err(RunError::new_failed("seq is zero length"));
    }

    for (canonical_base, method) in methods.iter() {
        let converter = DeltaListConverter::new_from_record(
            &record,
            canonical_base.char(),
        )?;
        let probs_for_positions = base_mod_probs_from_record(
            &record,
            &converter,
            canonical_base.char(),
        )?;
        let collapsed_probs_for_positions =
            collapse_mod_probs(probs_for_positions, method);
        let (mm, ml) = format_mm_ml_tag(
            collapsed_probs_for_positions,
            canonical_base.char(),
            &converter,
        );

        record
            .remove_aux("MM".as_bytes())
            .expect("failed to remove MM tag");
        record
            .remove_aux("ML".as_bytes())
            .expect("failed to remove ML tag");
        let mm = Aux::String(&mm);
        let ml_arr: AuxArray<u8> = {
            let sl = &ml;
            sl.into()
        };
        let ml = Aux::ArrayU8(ml_arr);
        record
            .push_aux("MM".as_bytes(), mm)
            .expect("failed to add MM tag");
        record
            .push_aux("ML".as_bytes(), ml)
            .expect("failed to add ML tag");
    }
    Ok(record)
}

impl Adjust {
    pub fn run(&self) -> Result<(), String> {
        let _handle = init_logging(self.log_filepath.as_ref());
        let fp = &self.in_bam;
        let out_fp = &self.out_bam;
        // let canonical_base = self.canonical_base;
        let mut reader =
            bam::Reader::from_path(fp).map_err(|e| e.to_string())?;
        let threads = self.threads;
        reader.set_threads(threads).map_err(|e| e.to_string())?;
        let header = bam::Header::from_template(reader.header());
        let mut out_bam =
            bam::Writer::from_path(out_fp, &header, bam::Format::Bam)
                .map_err(|e| e.to_string())?;

        let fail_fast = self.fail_fast;

        let methods = match (&self.method, &self.convert, &self.ignore) {
            (_, Some(convert), _) => {
                let mut conversions = HashMap::new();
                for chunk in convert.chunks(2) {
                    assert_eq!(chunk.len(), 2);
                    let from = ModCode::parse_raw_mod_code(chunk[0])?;
                    let to = ModCode::parse_raw_mod_code(chunk[1])?;
                    let froms = conversions.entry(to).or_insert(HashSet::new());
                    froms.insert(from);
                }
                for (to_code, from_codes) in conversions.iter() {
                    info!(
                        "Converting {} to {}",
                        from_codes.iter().map(|c| c.char()).collect::<String>(),
                        to_code.char()
                    )
                }
                conversions
                    .into_iter()
                    .map(|(to_mod_code, from_mod_codes)| {
                        let canonical_base = to_mod_code.canonical_base();
                        let method = CollapseMethod::Convert {
                            to: to_mod_code,
                            from: from_mod_codes,
                        };

                        (canonical_base, method)
                    })
                    .collect::<Vec<(DnaBase, CollapseMethod)>>()
            }
            (method, _, Some(raw_mod_code_to_ignore)) => {
                let mod_code_to_remove =
                    ModCode::parse_raw_mod_code(*raw_mod_code_to_ignore)?;
                info!(
                    "{}",
                    format!(
                        "Removing mod base {} from {}, new bam {}",
                        mod_code_to_remove.char(),
                        fp.to_str().unwrap_or("???"),
                        out_fp.to_str().unwrap_or("???")
                    )
                );
                let canonical_base = mod_code_to_remove.canonical_base();
                let method =
                    CollapseMethod::parse_str(method, mod_code_to_remove)?;
                vec![(canonical_base, method)]
            }
            _ => return Err(format!("specify convert or ignore")),
        };

        let spinner = get_spinner();
        spinner.set_message("Adjusting ModBAM");
        let mut total = 0usize;
        let mut total_failed = 0usize;
        let mut total_skipped = 0usize;
        for (i, result) in reader.records().enumerate() {
            if let Ok(record) = result {
                match adjust_mod_probs(record, &methods) {
                    Err(RunError::BadInput(InputError(err)))
                    | Err(RunError::Failed(err)) => {
                        if fail_fast {
                            return Err(err.to_string());
                        } else {
                            total_failed += 1;
                        }
                    }
                    Err(RunError::Skipped(_reason)) => {
                        total_skipped += 1;
                    }
                    Ok(record) => {
                        if let Err(err) = out_bam.write(&record) {
                            if fail_fast {
                                return Err(format!(
                                    "failed to write {}",
                                    err.to_string()
                                ));
                            } else {
                                total_failed += 1;
                            }
                        } else {
                            spinner.inc(1);
                            total = i;
                        }
                    }
                }
            } else {
                if fail_fast {
                    let err = result.err().unwrap().to_string();
                    return Err(err);
                }
                total_failed += 1;
            }
        }
        spinner.finish_and_clear();

        info!(
            "done, {} records processed, {} failed, {} skipped",
            total, total_failed, total_skipped
        );
        Ok(())
    }
}

#[derive(Args)]
pub struct ModBamPileup {
    /// Input BAM, should be sorted and have associated index
    in_bam: PathBuf,
    /// Output file
    out_bed: PathBuf,
    /// Number of threads to use while processing chunks concurrently.
    #[arg(short, long, default_value_t = 4)]
    threads: usize,

    /// Interval chunk size to process concurrently. Smaller interval chunk
    /// sizes will use less memory but incur more overhead.
    #[arg(short = 'i', long, default_value_t = 100_000)]
    interval_size: u32,

    /// Sample this fraction of the reads when estimating the
    /// `filter-percentile`. In practice, 50-100 thousand reads is sufficient to
    /// estimate the model output distribution and determine the filtering
    /// threshold.
    #[arg(short = 'f', long, default_value_t = 0.1)]
    sampling_frac: f64,

    /// random seed for deterministic running, default is non-deterministic
    #[arg(long)]
    seed: Option<u64>,

    /// Do not perform any filtering, include all mod base calls in output
    #[arg(group = "thresholds", long, default_value_t = false)]
    no_filtering: bool,

    /// Filter (remove) mod-calls where the probability of the predicted
    /// variant is below this percentile. For example, 0.1 will filter
    /// out the lowest 10% of modification calls.
    #[arg(group = "thresholds", short = 'p', long, default_value_t = 0.1)]
    filter_percentile: f32,

    /// Filter threshold, drop calls below this probability
    #[arg(group = "thresholds", long)]
    filter_threshold: Option<f32>,

    /// Output debug logs to file at this path
    #[arg(long)]
    log_filepath: Option<PathBuf>,

    /// Combine mod calls, all counts of modified bases are summed together.
    #[arg(long, default_value_t = false, group = "combine_args")]
    combine: bool,

    /// Secret API: collapse _in_situ_. Arg is the method to use {'norm', 'dist'}.
    #[arg(long, group = "combine_args", hide = true, value_parser)]
    collapse: Option<char>,
    /// Method to use to collapse mod calls, 'norm', 'dist'.
    #[arg(
        long,
        default_value_t = String::from("norm"),
        value_parser = check_collapse_method,
        requires = "collapse",
        hide = true,
    )]
    #[arg(long)]
    method: String,

    /// Output BED format (for visualization)
    #[arg(long, default_value_t = false)]
    output_bed: bool,
}

impl ModBamPileup {
    fn run(&self) -> AnyhowResult<(), String> {
        let _handle = init_logging(self.log_filepath.as_ref());
        // do this first so we fail when the file isn't readable
        let header = bam::IndexedReader::from_path(&self.in_bam)
            .map_err(|e| e.to_string())
            .map(|reader| reader.header().to_owned())?;

        let pileup_options = match (self.combine, &self.collapse) {
            (false, None) => PileupNumericOptions::Passthrough,
            (true, _) => PileupNumericOptions::Combine,
            (_, Some(raw_mod_code)) => {
                let mod_code = ModCode::parse_raw_mod_code(*raw_mod_code)?;
                let method =
                    CollapseMethod::parse_str(self.method.as_str(), mod_code)?;
                PileupNumericOptions::Collapse(method)
            }
        };

        let threshold = if self.no_filtering {
            0f32
        } else if let Some(user_threshold) = self.filter_threshold {
            info!(
                "Using user-defined threshold probability: {}",
                user_threshold
            );
            user_threshold
        } else {
            info!(
                "Determining filter threshold probability using sampling \
                frequency {}",
                self.sampling_frac
            );
            // todo need to calc threshold based on collapsed probs
            calc_threshold_from_bam(
                &self.in_bam,
                self.threads,
                self.sampling_frac,
                self.filter_percentile,
                self.seed,
            )?
        };

        info!("Using filter threshold {}", threshold);

        let tids = (0..header.target_count())
            .filter_map(|tid| {
                let chrom_name =
                    String::from_utf8(header.tid2name(tid).to_vec()).unwrap_or("???".to_owned());
                match header.target_len(tid) {
                    Some(size) => Some((tid, size as u32, chrom_name)),
                    None => {
                        debug!("> no size information for {chrom_name} (tid: {tid})");
                        None
                    }
                }
            })
            .collect::<Vec<(u32, u32, String)>>();

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.threads)
            .build()
            .with_context(|| "failed to make threadpool")
            .map_err(|e| e.to_string())?;

        let (snd, rx) = bounded(1_000); // todo figure out sane default for this?
        let in_bam_fp = self.in_bam.clone();
        let interval_size = self.interval_size;

        let master_progress = MultiProgress::new();
        let sty = ProgressStyle::with_template(
            "[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}",
        )
        .unwrap()
        .progress_chars("##-");
        let tid_progress = master_progress
            .add(ProgressBar::new(tids.len() as u64))
            .with_style(sty.clone());
        tid_progress.set_message("contigs");
        let write_progress = master_progress.add(get_spinner());
        write_progress.set_message("rows written");

        thread::spawn(move || {
            pool.install(|| {
                for (tid, size, ref_name) in tids {
                    let intervals = IntervalChunks::new(size, interval_size, 0)
                        .collect::<Vec<(u32, u32)>>();
                    let n_intervals = intervals.len();
                    let interval_progress = master_progress.add(
                        ProgressBar::new(n_intervals as u64)
                            .with_style(sty.clone()),
                    );
                    interval_progress
                        .set_message(format!("processing {}", ref_name));
                    let mut result: Vec<Result<ModBasePileup, String>> = vec![];
                    let (res, _) = rayon::join(
                        || {
                            intervals
                                .into_par_iter()
                                .progress_with(interval_progress)
                                .map(|(start, end)| {
                                    process_region(
                                        &in_bam_fp,
                                        tid,
                                        start,
                                        end,
                                        threshold,
                                        &pileup_options,
                                    )
                                })
                                .collect::<Vec<Result<ModBasePileup, String>>>()
                        },
                        || {
                            result.into_iter().for_each(|mod_base_pileup| {
                                snd.send(mod_base_pileup)
                                    .expect("failed to send")
                            });
                        },
                    );
                    result = res;
                    result.into_iter().for_each(|pileup| {
                        snd.send(pileup).expect("failed to send")
                    });
                    tid_progress.inc(1);
                }
                tid_progress.finish_and_clear();
            });
        });

        let out_fp_str = self.out_bed.clone();
        let out_fp = std::fs::File::create(out_fp_str)
            .context("failed to make output file")
            .map_err(|e| e.to_string())?;
        let mut writer: Box<dyn OutWriter<ModBasePileup>> = if self.output_bed {
            Box::new(BEDWriter::new(BufWriter::new(out_fp)))
        } else {
            Box::new(BedMethylWriter::new(BufWriter::new(out_fp)))
        };
        for result in rx.into_iter() {
            match result {
                Ok(mod_base_pileup) => {
                    let rows_written = writer.write(mod_base_pileup)?;
                    write_progress.inc(rows_written);
                }
                Err(message) => {
                    debug!("> unexpected error {message}");
                }
            }
        }
        let rows_processed = write_progress.position();
        write_progress.finish_and_clear();
        info!("Done, processed {rows_processed} rows.");
        Ok(())
    }
}

fn parse_percentiles(
    raw_percentiles: &str,
) -> Result<Vec<f32>, ParseFloatError> {
    if raw_percentiles.contains("..") {
        todo!("handle parsing ranges")
    } else {
        raw_percentiles
            .split(',')
            .map(|x| x.parse::<f32>())
            .collect()
    }
}

#[derive(Args)]
pub struct SampleModBaseProbs {
    /// Input BAM, should be sorted and have associated index
    in_bam: PathBuf,
    /// Sample fraction
    #[arg(short = 'f', long, default_value_t = 0.1)]
    sampling_frac: f64,
    /// number of threads to use reading BAM
    #[arg(short, long, default_value_t = 4)]
    threads: usize,
    /// random seed for deterministic running, default is non-deterministic
    #[arg(short, long)]
    seed: Option<u64>,
    /// Percentiles to calculate, space separated list
    #[arg(short, long, default_value_t=String::from("0.1,0.5,0.9"))]
    percentiles: String,
}

impl SampleModBaseProbs {
    fn run(&self) -> AnyhowResult<(), String> {
        let mut bam = bam::Reader::from_path(&self.in_bam).unwrap();
        bam.set_threads(self.threads).unwrap();

        let mut probs =
            sample_modbase_probs(&mut bam, self.seed, self.sampling_frac)
                .map_err(|e| e.to_string())?;
        let desired_percentiles = parse_percentiles(&self.percentiles)
            .with_context(|| {
                format!("failed to parse percentiles: {}", &self.percentiles)
            })
            .map_err(|e| e.to_string())?;
        println!(
            "{}",
            Percentiles::new(&mut probs, &desired_percentiles)?.report()
        );
        Ok(())
    }
}

#[derive(Args)]
pub struct ModSummarize {
    /// Input ModBam file
    in_bam: PathBuf,
    /// number of threads to use reading BAM
    #[arg(short, long, default_value_t = 4)]
    threads: usize,
}

impl ModSummarize {
    pub fn run(&self) -> AnyhowResult<(), String> {
        let mod_summary = summarize_modbam(&self.in_bam, self.threads)
            .map_err(|e| e.to_string())?;
        let mut writer = TsvWriter::new();
        writer.write(mod_summary).map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[derive(Args)]
pub struct MotifBed {
    /// Input FASTA file
    fasta: PathBuf,
    /// Motif to search for within FASTA
    motif: String,
    /// Offset within motif
    offset: usize,
}

impl MotifBed {
    fn run(&self) -> Result<(), String> {
        motif_bed(&self.fasta, &self.motif, self.offset);
        Ok(())
    }
}
