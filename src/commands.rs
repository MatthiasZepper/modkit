use std::collections::{HashMap, HashSet};
use std::io::BufWriter;
use std::num::ParseFloatError;
use std::path::PathBuf;
use std::thread;

use anyhow::{anyhow, Context, Result as AnyhowResult};
use clap::{Args, Subcommand, ValueEnum};
use crossbeam_channel::bounded;
use indicatif::{
    MultiProgress, ParallelProgressIterator, ProgressBar, ProgressStyle,
};
use log::{debug, error, info, warn};
use rayon::prelude::*;
use rust_htslib::bam;
use rust_htslib::bam::record::{Aux, AuxArray};
use rust_htslib::bam::Read;

use crate::errs::{InputError, RunError};
use crate::interval_chunks::IntervalChunks;
use crate::logging::init_logging;
use crate::mod_bam::{
    collapse_mod_probs, format_mm_ml_tag, CollapseMethod, ModBaseInfo,
    SkipMode, ML_TAGS, MM_TAGS,
};
use crate::mod_base_code::ModCode;
use crate::mod_pileup::{process_region, ModBasePileup, PileupNumericOptions};
use crate::motif_bed::{motif_bed, MotifLocations, RegexMotif};
use crate::summarize::summarize_modbam;
use crate::thresholds::{
    calc_threshold_from_bam, get_modbase_probs_from_bam, Percentiles,
};
use crate::util;
use crate::util::{
    add_modkit_pg_records, get_spinner, get_targets, record_is_secondary,
    Region,
};
use crate::writers::{BedGraphWriter, BedMethylWriter, OutWriter, TsvWriter};

#[derive(Subcommand)]
pub enum Commands {
    /// Tabulates base modification calls across genomic positions. This command
    /// produces a bedMethyl formatted file. Schema and description of fields can
    /// be found in the README.
    Pileup(ModBamPileup),
    /// Performs various operations on BAM files containing base modification
    /// information, such as converting base modification codes and ignoring
    /// modification calls. Produces a BAM output file.
    AdjustMods(Adjust),
    /// Renames Mm/Ml to tags to MM/ML. Also allows changing the the mode flag from
    /// silent '.' to explicitly '?' or '.'.
    UpdateTags(Update),
    /// Calculate an estimate of the base modification probability distribution.
    SampleProbs(SampleModBaseProbs),
    /// Summarize the mod tags present in a BAM and get basic statistics
    Summary(ModSummarize),
    /// Create BED file with all locations of a sequence motif
    MotifBed(MotifBed),
}

impl Commands {
    pub fn run(&self) -> Result<(), String> {
        match self {
            Self::AdjustMods(x) => x.run().map_err(|e| e.to_string()),
            Self::Pileup(x) => x.run().map_err(|e| e.to_string()),
            Self::SampleProbs(x) => x.run().map_err(|e| e.to_string()),
            Self::Summary(x) => x.run(),
            Self::MotifBed(x) => x.run(),
            Self::UpdateTags(x) => x.run(),
        }
    }
}

fn get_threshold_from_options(
    in_bam: &PathBuf,
    threads: usize,
    interval_size: u32,
    sample_frac: Option<f64>,
    num_reads: usize,
    no_filtering: bool,
    filter_percentile: f32,
    seed: Option<u64>,
    user_threshold: Option<f32>,
    region: Option<&Region>,
) -> AnyhowResult<f32> {
    if no_filtering {
        info!("not performing filtering");
        return Ok(0f32);
    }
    if let Some(t) = user_threshold {
        return Ok(t);
    }
    let (sample_frac, num_reads) = match sample_frac {
        Some(f) => {
            let pct = f * 100f64;
            info!("sampling {pct}% of reads");
            (Some(f), None)
        }
        None => {
            info!("sampling {num_reads} reads from BAM");
            (None, Some(num_reads))
        }
    };
    calc_threshold_from_bam(
        in_bam,
        threads,
        interval_size,
        sample_frac,
        num_reads,
        filter_percentile,
        seed,
        region,
    )
}

#[derive(Args)]
pub struct Adjust {
    /// BAM file to collapse mod call from.
    in_bam: PathBuf,
    /// File path to new BAM file to be created.
    out_bam: PathBuf,
    /// Modified base code to ignore/remove, see
    /// https://samtools.github.io/hts-specs/SAMtags.pdf for details on
    /// the modified base codes.
    #[arg(long, conflicts_with = "convert", default_value_t = 'h')]
    ignore: char,
    /// Number of threads to use.
    #[arg(short, long, default_value_t = 4)]
    threads: usize,
    /// Fast fail, stop processing at the first invalid sequence record. Default
    /// behavior is to continue and report failed/skipped records at the end.
    #[arg(short, long = "ff", default_value_t = false)]
    fail_fast: bool,
    /// Convert one mod-tag to another, summing the probabilities together if
    /// the retained mod tag is already present.
    #[arg(group = "prob_args", long, action = clap::ArgAction::Append, num_args = 2)]
    convert: Option<Vec<char>>,
    /// Output debug logs to file at this path.
    #[arg(long)]
    log_filepath: Option<PathBuf>,
}

type CliResult<T> = Result<T, RunError>;

fn record_is_valid(record: &bam::Record) -> Result<(), RunError> {
    if record_is_secondary(&record) {
        return Err(RunError::new_skipped("not primary"));
    }
    if record.seq_len() == 0 {
        return Err(RunError::new_failed("seq is zero length"));
    }
    Ok(())
}

fn adjust_mod_probs(
    mut record: bam::Record,
    methods: &[CollapseMethod],
) -> CliResult<bam::Record> {
    let _ok = record_is_valid(&record)?;

    let mod_base_info = ModBaseInfo::new_from_record(&record)?;
    let mm_style = mod_base_info.mm_style;
    let ml_style = mod_base_info.ml_style;

    let mut mm_agg = String::new();
    let mut ml_agg = Vec::new();

    let (converters, mod_prob_iter) = mod_base_info.into_iter_base_mod_probs();
    for (base, strand, mut seq_pos_mod_probs) in mod_prob_iter {
        let converter = converters.get(&base).unwrap();
        for method in methods {
            seq_pos_mod_probs = collapse_mod_probs(seq_pos_mod_probs, method);
        }
        let (mm, mut ml) =
            format_mm_ml_tag(seq_pos_mod_probs, strand, converter);
        mm_agg.push_str(&mm);
        ml_agg.extend_from_slice(&mut ml);
    }

    record
        .remove_aux(mm_style.as_bytes())
        .expect("failed to remove MM tag");
    record
        .remove_aux(ml_style.as_bytes())
        .expect("failed to remove ML tag");
    let mm = Aux::String(&mm_agg);
    let ml_arr: AuxArray<u8> = {
        let sl = &ml_agg;
        sl.into()
    };
    let ml = Aux::ArrayU8(ml_arr);
    record
        .push_aux(mm_style.as_bytes(), mm)
        .expect("failed to add MM tag");
    record
        .push_aux(ml_style.as_bytes(), ml)
        .expect("failed to add ML tag");

    Ok(record)
}

impl Adjust {
    pub fn run(&self) -> AnyhowResult<()> {
        let _handle = init_logging(self.log_filepath.as_ref());
        let fp = &self.in_bam;
        let out_fp = &self.out_bam;
        let mut reader = bam::Reader::from_path(fp)?;
        let threads = self.threads;
        reader.set_threads(threads)?;
        let mut header = bam::Header::from_template(reader.header());
        add_modkit_pg_records(&mut header);
        let mut out_bam =
            bam::Writer::from_path(out_fp, &header, bam::Format::Bam)?;

        let fail_fast = self.fail_fast;

        let methods = if let Some(convert) = &self.convert {
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
                    let method = CollapseMethod::Convert {
                        to: to_mod_code,
                        from: from_mod_codes,
                    };

                    method
                })
                .collect::<Vec<CollapseMethod>>()
        } else {
            let mod_code_to_remove = ModCode::parse_raw_mod_code(self.ignore)?;
            info!(
                "{}",
                format!(
                    "Removing mod base {} from {}, new bam {}",
                    mod_code_to_remove.char(),
                    fp.to_str().unwrap_or("???"),
                    out_fp.to_str().unwrap_or("???")
                )
            );
            let method = CollapseMethod::ReDistribute(mod_code_to_remove);
            vec![method]
        };

        let spinner = get_spinner();
        spinner.set_message("Adjusting ModBAM");
        let mut total = 0usize;
        let mut total_failed = 0usize;
        let mut total_skipped = 0usize;
        for (i, result) in reader.records().enumerate() {
            if let Ok(record) = result {
                let record_name = util::get_query_name_string(&record)
                    .unwrap_or("???".to_owned());
                match adjust_mod_probs(record, &methods) {
                    Err(RunError::BadInput(InputError(err)))
                    | Err(RunError::Failed(err)) => {
                        if fail_fast {
                            return Err(anyhow!("{}", err.to_string()));
                        } else {
                            debug!("read {} failed, {}", record_name, err);
                            total_failed += 1;
                        }
                    }
                    Err(RunError::Skipped(_reason)) => {
                        total_skipped += 1;
                    }
                    Ok(record) => {
                        if let Err(err) = out_bam.write(&record) {
                            if fail_fast {
                                return Err(anyhow!(
                                    "failed to write {}",
                                    err.to_string()
                                ));
                            } else {
                                debug!("failed to write {}", err);
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
                    return Err(anyhow!("{}", err));
                }
                total_failed += 1;
            }
        }
        spinner.finish_and_clear();

        info!(
            "done, {} records processed, {} failed, {} skipped",
            total + 1,
            total_failed,
            total_skipped
        );
        Ok(())
    }
}

#[derive(Args)]
pub struct ModBamPileup {
    // running args
    /// Input BAM, should be sorted and have associated index available.
    in_bam: PathBuf,
    /// Output file (or directory with --bedgraph option) to write results into.
    out_bed: PathBuf,
    /// Specify a file for debug logs to be written to, otherwise ignore them.
    /// Setting a file is recommended.
    #[arg(long)]
    log_filepath: Option<PathBuf>,
    /// Process only the specified region of the BAM when performing pileup.
    /// Format should be <chrom_name>:<start>-<end> or <chrom_name>.
    #[arg(long)]
    region: Option<String>,

    // processing args
    /// Number of threads to use while processing chunks concurrently.
    #[arg(short, long, default_value_t = 4)]
    threads: usize,
    /// Interval chunk size to process concurrently. Smaller interval chunk
    /// sizes will use less memory but incur more overhead.
    #[arg(
        short = 'i',
        long,
        default_value_t = 100_000,
        hide_short_help = true
    )]
    interval_size: u32,

    // sampling args
    /// Sample this many reads when estimating the filtering threshold. Reads will
    /// be sampled evenly across aligned genome. If a region is specified, either with
    /// the --region option or the --sample-region option, then reads will be sampled
    /// evenly across the region given. This option is useful for large BAM files.
    /// In practice, 10-50 thousand reads is sufficient to estimate the model output
    /// distribution and determine the filtering threshold.
    #[arg(
        group = "sampling_options",
        short = 'n',
        long,
        default_value_t = 10_042
    )]
    num_reads: usize,
    /// Sample this fraction of the reads when estimating the filter-percentile.
    /// In practice, 50-100 thousand reads is sufficient to estimate the model output
    /// distribution and determine the filtering threshold. See filtering.md for
    /// details on filtering.
    #[arg(
        group = "sampling_options",
        short = 'f',
        long,
        hide_short_help = true
    )]
    sampling_frac: Option<f64>,
    /// Set a random seed for deterministic running, the default is non-deterministic.
    #[arg(
        long,
        conflicts_with = "num_reads",
        requires = "sampling_frac",
        hide_short_help = true
    )]
    seed: Option<u64>,
    /// Do not perform any filtering, include all mod base calls in output. See
    /// filtering.md for details on filtering.
    #[arg(group = "thresholds", long, default_value_t = false)]
    no_filtering: bool,
    /// Filter out modified base calls where the probability of the predicted
    /// variant is below this confidence percentile. For example, 0.1 will filter
    /// out the 10% lowest confidence modification calls.
    #[arg(
        group = "thresholds",
        short = 'p',
        long,
        default_value_t = 0.1,
        hide_short_help = true
    )]
    filter_percentile: f32,
    /// Use a specific filter threshold, drop calls below this probability.
    #[arg(group = "thresholds", long, hide_short_help = true)]
    filter_threshold: Option<f32>,
    /// Specify a region for sampling reads from when estimating the threshold probability.
    /// If this option is not provided, but --region is provided, the genomic interval
    /// passed to --region will be used.
    /// Format should be <chrom_name>:<start>-<end> or <chrom_name>.
    #[arg(long)]
    sample_region: Option<String>,
    /// Interval chunk size to process concurrently when estimating the threshold
    /// probability, can be larger than the pileup processing interval.
    #[arg(long, default_value_t = 1_000_000, hide_short_help = true)]
    sampling_interval_size: u32,

    // collapsing and combining args
    /// Ignore a modified base class  _in_situ_ by redistributing base modification
    /// probability equally across other options. For example, if collapsing 'h',
    /// with 'm' and canonical options, half of the probability of 'h' will be added to
    /// both 'm' and 'C'. A full description of the methods can be found in
    /// collapse.md.
    #[arg(long, group = "combine_args", hide_short_help = true)]
    ignore: Option<char>,
    /// Force allow implicit-canonical mode. By default modkit does not allow
    /// pileup with the implicit mode ('.', or silent). The `update-tags`
    /// subcommand is provided to update tags to the new mode. This option allows
    /// the interpretation of implicit mode tags: residues without modified
    /// base probability will be interpreted as being the non-modified base.
    /// We do not recommend using this option.
    #[arg(
        long,
        hide_short_help = true,
        default_value_t = false,
        hide_short_help = true
    )]
    force_allow_implicit: bool,
    /// Only output counts at CpG motifs. Requires a reference sequence to be
    /// provided.
    #[arg(long, requires = "reference_fasta", default_value_t = false)]
    cpg: bool,
    /// Reference sequence in FASTA format. Required for CpG motif filtering.
    #[arg(long = "ref")]
    reference_fasta: Option<PathBuf>,
    /// Optional preset options for specific applications.
    /// traditional: Prepares bedMethyl analogous to that generated from other technologies
    /// for the analysis of 5mC modified bases. Shorthand for --cpg --combine-strands
    /// --ignore h.
    #[arg(
        long,
        requires = "reference_fasta",
        conflicts_with_all = ["combine_mods", "cpg", "combine_strands", "ignore"],
    )]
    preset: Option<Presets>,
    /// Combine base modification calls, all counts of modified bases are summed together. See
    /// collapse.md for details.
    #[arg(
        long,
        default_value_t = false,
        group = "combine_args",
        hide_short_help = true
    )]
    combine_mods: bool,
    /// When performing CpG analysis, sum the counts from the positive and
    /// negative strands into the counts for the positive strand.
    #[arg(long, requires = "cpg", default_value_t = false)]
    combine_strands: bool,

    // output args
    /// For bedMethyl output, separate columns with only tabs. The default is
    /// to use tabs for the first 10 fields and spaces thereafter. The
    /// default behavior is more likely to be compatible with genome viewers.
    /// Enabling this option may make it easier to parse the output with
    /// tabular data handlers that expect a single kind of separator.
    #[arg(
        long,
        conflicts_with = "bedgraph",
        default_value_t = false,
        hide_short_help = true
    )]
    only_tabs: bool,
    /// Output bedGraph format, see https://genome.ucsc.edu/goldenPath/help/bedgraph.html.
    /// For this setting, specify a directory for output files to be make in.
    /// Two files for each modification will be produced, one for the positive strand
    /// and one for the negative strand. So for 5mC (m) and 5hmC (h) there will be 4 files
    /// produced.
    #[arg(
        long,
        conflicts_with = "only_tabs",
        default_value_t = false,
        hide_short_help = true
    )]
    bedgraph: bool,
    /// Prefix to prepend on bedgraph output file names. Without this option the files
    /// will be <mod_code>_<strand>.bedgraph
    #[arg(long, requires = "bedgraph")]
    prefix: Option<String>,
}

impl ModBamPileup {
    fn run(&self) -> AnyhowResult<()> {
        let _handle = init_logging(self.log_filepath.as_ref());
        // do this first so we fail when the file isn't readable
        let header = bam::IndexedReader::from_path(&self.in_bam)
            .map(|reader| reader.header().to_owned())?;
        let region = if let Some(raw_region) = &self.region {
            info!("parsing region {raw_region}");
            Some(Region::parse_str(raw_region, &header)?)
        } else {
            None
        };
        let sampling_region = if let Some(raw_region) = &self.sample_region {
            info!("parsing sample region {raw_region}");
            Some(Region::parse_str(raw_region, &header)?)
        } else {
            None
        };

        let (pileup_options, combine_strands) = match self.preset {
            Some(Presets::traditional) => (
                PileupNumericOptions::Collapse(CollapseMethod::ReDistribute(
                    ModCode::h,
                )),
                true,
            ),
            None => {
                let options = match (self.combine_mods, &self.ignore) {
                    (false, None) => PileupNumericOptions::Passthrough,
                    (true, _) => PileupNumericOptions::Combine,
                    (_, Some(raw_mod_code)) => {
                        let mod_code =
                            ModCode::parse_raw_mod_code(*raw_mod_code)?;
                        let method = CollapseMethod::ReDistribute(mod_code);
                        PileupNumericOptions::Collapse(method)
                    }
                };
                (options, self.combine_strands)
            }
        };

        // setup the writer here so we fail before doing any work (if there are problems).
        let out_fp_str = self.out_bed.clone();
        let mut writer: Box<dyn OutWriter<ModBasePileup>> = if self.bedgraph {
            Box::new(BedGraphWriter::new(out_fp_str, self.prefix.as_ref())?)
        } else {
            let out_fp = std::fs::File::create(out_fp_str)
                .context("failed to make output file")?;
            Box::new(BedMethylWriter::new(
                BufWriter::new(out_fp),
                self.only_tabs,
            ))
        };

        let threshold = get_threshold_from_options(
            &self.in_bam,
            self.threads,
            self.sampling_interval_size,
            self.sampling_frac,
            self.num_reads,
            self.no_filtering,
            self.filter_percentile,
            self.seed,
            self.filter_threshold,
            sampling_region.as_ref().or(region.as_ref()),
        )?;

        match (threshold * 100f32).ceil() as usize {
            0..=60 => error!(
                "Threshold of {threshold} is very low. Consider increasing the \
                filter-percentile or specifying a higher threshold."),
            61..=70 => warn!(
                "Threshold of {threshold} is low. Consider increasing the \
                filter-percentile or specifying a higher threshold."
            ),
            _ => info!("Using filter threshold {}.", threshold),
        }

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.threads)
            .build()
            .with_context(|| "failed to make threadpool")?;

        let tids = get_targets(&header, region.as_ref());
        let use_cpg_motifs = self.cpg
            || self
                .preset
                .map(|preset| match preset {
                    Presets::traditional => true,
                })
                .unwrap_or(false);
        let (motif_locations, tids) = if use_cpg_motifs {
            let fasta_fp = self
                .reference_fasta
                .as_ref()
                .ok_or(anyhow!("reference fasta is required for CpG"))?;
            let regex_motif = RegexMotif::parse_string("CG", 0).unwrap();
            debug!("filtering output to only CpG motifs");
            if combine_strands {
                debug!("combining + and - strand counts");
            }
            let names_to_tid = tids
                .iter()
                .map(|target| (target.name.as_str(), target.tid))
                .collect::<HashMap<&str, u32>>();
            let motif_locations = pool.install(|| {
                MotifLocations::from_fasta(fasta_fp, regex_motif, &names_to_tid)
            })?;
            let filtered_tids = motif_locations.filter_reference_records(tids);
            (Some(motif_locations), filtered_tids)
        } else {
            (None, tids)
        };

        let (snd, rx) = bounded(1_000); // todo figure out sane default for this?
        let in_bam_fp = self.in_bam.clone();
        let interval_size = self.interval_size;

        let master_progress = MultiProgress::new();
        let sty = ProgressStyle::with_template(
            "[{elapsed_precise}] {bar:40.green/yellow} {pos:>7}/{len:7} {msg}",
        )
        .unwrap()
        .progress_chars("##-");
        let tid_progress = master_progress
            .add(ProgressBar::new(tids.len() as u64))
            .with_style(sty.clone());
        tid_progress.set_message("contigs");
        let write_progress = master_progress.add(get_spinner());
        write_progress.set_message("rows written");

        let force_allow = self.force_allow_implicit;

        let interval_style = ProgressStyle::with_template(
            "[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}",
        )
        .unwrap()
        .progress_chars("##-");

        thread::spawn(move || {
            pool.install(|| {
                for target in tids {
                    let intervals = IntervalChunks::new(
                        target.start,
                        target.length,
                        interval_size,
                        target.tid,
                        motif_locations.as_ref(),
                    )
                    .collect::<Vec<(u32, u32)>>();
                    let n_intervals = intervals.len();
                    let interval_progress = master_progress.add(
                        ProgressBar::new(n_intervals as u64)
                            .with_style(interval_style.clone()),
                    );
                    interval_progress
                        .set_message(format!("processing {}", &target.name));
                    let mut result: Vec<Result<ModBasePileup, String>> = vec![];
                    let (res, _) = rayon::join(
                        || {
                            intervals
                                .into_par_iter()
                                .progress_with(interval_progress)
                                .map(|(start, end)| {
                                    process_region(
                                        &in_bam_fp,
                                        target.tid,
                                        start,
                                        end,
                                        threshold,
                                        &pileup_options,
                                        force_allow,
                                        combine_strands,
                                        motif_locations.as_ref(),
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
    /// Input BAM with modified base tags. If a index is found
    /// reads will be sampled evenly across the length of the
    /// reference sequence.
    in_bam: PathBuf,
    /// Max number of reads to use, especially recommended when using a large
    /// BAM without an index. If an indexed BAM is provided, the reads will be
    /// sampled evenly over the length of the aligned reference. If a region is
    /// passed with the --region option, they will be sampled over the genomic
    /// region.
    #[arg(
        group = "sampling_options",
        short = 'n',
        long,
        default_value_t = 10_042
    )]
    num_reads: usize,
    /// Fraction of reads to sample, for example 0.1 will sample
    /// 1/10th of the reads.
    #[arg(group = "sampling_options", short = 'f', long)]
    sampling_frac: Option<f64>,
    /// Interval chunk size to process concurrently. Smaller interval chunk
    /// sizes will use less memory but incur more overhead. Only used when
    /// sampling probs from an indexed bam.
    #[arg(short = 'i', long, default_value_t = 1_000_000)]
    interval_size: u32,
    /// Do not perform any filtering, include all mod base calls in output. See
    /// filtering.md for details on filtering.
    #[arg(group = "sampling_options", long, default_value_t = false)]
    no_filtering: bool,
    /// Process only the specified region of the BAM when performing pileup.
    /// Format should be <chrom_name>:<start>-<end>.
    #[arg(long)]
    region: Option<String>,
    /// Number of threads to use.
    #[arg(short, long, default_value_t = 4)]
    threads: usize,
    /// Random seed for deterministic running, the default is non-deterministic.
    #[arg(short, long)]
    seed: Option<u64>,
    /// Percentiles to calculate, a space separated list of floats.
    #[arg(short, long, default_value_t=String::from("0.1,0.5,0.9"))]
    percentiles: String,
    /// Specify a file for debug logs to be written to, otherwise ignore them.
    /// Setting a file is recommended.
    #[arg(long)]
    log_filepath: Option<PathBuf>,
}

impl SampleModBaseProbs {
    fn run(&self) -> AnyhowResult<()> {
        let _handle = init_logging(self.log_filepath.as_ref());
        let reader = bam::Reader::from_path(&self.in_bam)?;

        let region = if let Some(raw_region) = &self.region {
            info!("parsing region {raw_region}");
            Some(Region::parse_str(raw_region, reader.header())?)
        } else {
            None
        };
        let (sample_frac, num_reads) = match (
            self.no_filtering,
            self.sampling_frac,
            self.num_reads,
        ) {
            (true, _, _) => {
                info!("performing no filtering");
                (None, None)
            }
            (false, Some(f), _) => {
                let pct = f * 100f64;
                info!("sampling {pct}% of reads to estimate probability distribution");
                (Some(f), None)
            }
            (false, _, num) => {
                info!("sampling {num} reads from BAM to estimate probability distribution");
                (None, Some(num))
            }
        };

        let desired_percentiles = parse_percentiles(&self.percentiles)
            .with_context(|| {
                format!("failed to parse percentiles: {}", &self.percentiles)
            })?;
        let mut probs = get_modbase_probs_from_bam(
            &self.in_bam,
            self.threads,
            self.interval_size,
            sample_frac,
            num_reads,
            self.seed,
            region.as_ref(),
        )?;
        println!(
            "{}",
            Percentiles::new(&mut probs, &desired_percentiles)?.report()
        );
        Ok(())
    }
}

#[derive(Args)]
pub struct ModSummarize {
    /// Input ModBam file.
    in_bam: PathBuf,
    /// Number of threads to use reading BAM.
    #[arg(short, long, default_value_t = 4)]
    threads: usize,
    /// Specify a file for debug logs to be written to, otherwise ignore them.
    /// Setting a file is recommended.
    #[arg(long)]
    log_filepath: Option<PathBuf>,

    /// Filter out mod-calls where the probability of the predicted
    /// variant is below this percentile. For example, 0.1 will filter
    /// out the 10% lowest confidence modification calls.
    /// Set a maximum number of reads to process.
    #[arg(short = 'n', long)]
    num_reads: Option<usize>,
    /// Sample this fraction of the reads when estimating the
    /// `filter-percentile`. In practice, 50-100 thousand reads is sufficient to
    /// estimate the model output distribution and determine the filtering
    /// threshold.
    #[arg(short = 'f', long, default_value_t = 0.1, hide_short_help = true)]
    sampling_frac: f64,
    /// Set a random seed for deterministic running, the default is non-deterministic.
    #[arg(long, hide_short_help = true)]
    seed: Option<u64>,
    /// Do not perform any filtering, include all mod base calls in output. See
    /// filtering.md for details on filtering.
    #[arg(group = "thresholds", long, default_value_t = false)]
    no_filtering: bool,

    /// Filter out modified base calls where the probability of the predicted
    /// variant is below this confidence percentile. For example, 0.1 will filter
    /// out the 10% lowest confidence modification calls.
    #[arg(group = "thresholds", short = 'p', long, default_value_t = 0.1)]
    filter_percentile: f32,
    /// Filter threshold, drop calls below this probability.
    #[arg(group = "thresholds", long)]
    filter_threshold: Option<f32>,
}

impl ModSummarize {
    pub fn run(&self) -> AnyhowResult<(), String> {
        let _handle = init_logging(self.log_filepath.as_ref());
        let mod_summary = summarize_modbam(
            &self.in_bam,
            self.threads,
            self.filter_threshold.unwrap_or(0f32),
            self.num_reads,
        )
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
    /// Offset within motif.
    offset: usize,
}

impl MotifBed {
    fn run(&self) -> Result<(), String> {
        motif_bed(&self.fasta, &self.motif, self.offset);
        Ok(())
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
#[allow(non_camel_case_types)]
enum Presets {
    traditional,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
#[allow(non_camel_case_types)]
enum ModMode {
    ambiguous,
    implicit,
}

impl ModMode {
    fn to_skip_mode(self) -> SkipMode {
        match self {
            Self::ambiguous => SkipMode::Ambiguous,
            Self::implicit => SkipMode::ProbModified,
        }
    }
}

#[derive(Args)]
pub struct Update {
    /// BAM file to update modified base tags in.
    in_bam: PathBuf,
    /// File path to new BAM file to be created.
    out_bam: PathBuf,
    /// Mode, change mode to this value, options {'ambiguous', 'implicit'}.
    /// See spec at: https://samtools.github.io/hts-specs/SAMtags.pdf.
    /// 'ambiguous' ('?') means residues without explicit modification
    /// probabilities will not be assumed canonical or modified. 'implicit'
    /// means residues without explicit modification probabilities are
    /// assumed to be canonical.
    #[arg(short, long, value_enum)]
    mode: Option<ModMode>,
    /// Number of threads to use.
    #[arg(short, long, default_value_t = 4)]
    threads: usize,
    /// Output debug logs to file at this path.
    #[arg(long)]
    log_filepath: Option<PathBuf>,
}

fn update_mod_tags(
    mut record: bam::Record,
    new_mode: Option<SkipMode>,
) -> CliResult<bam::Record> {
    let _ok = record_is_valid(&record)?;
    let mod_base_info = ModBaseInfo::new_from_record(&record)?;
    let mm_style = mod_base_info.mm_style;
    let ml_style = mod_base_info.ml_style;

    let mut mm_agg = String::new();
    let mut ml_agg = Vec::new();

    let (converters, mod_prob_iter) = mod_base_info.into_iter_base_mod_probs();
    for (base, strand, mut seq_pos_mod_probs) in mod_prob_iter {
        let converter = converters.get(&base).unwrap();
        if let Some(mode) = new_mode {
            seq_pos_mod_probs.skip_mode = mode;
        }
        let (mm, mut ml) =
            format_mm_ml_tag(seq_pos_mod_probs, strand, converter);
        mm_agg.push_str(&mm);
        ml_agg.extend_from_slice(&mut ml);
    }
    record
        .remove_aux(mm_style.as_bytes())
        .expect("failed to remove MM tag");
    record
        .remove_aux(ml_style.as_bytes())
        .expect("failed to remove ML tag");
    let mm = Aux::String(&mm_agg);
    let ml_arr: AuxArray<u8> = {
        let sl = &ml_agg;
        sl.into()
    };
    let ml = Aux::ArrayU8(ml_arr);
    record
        .push_aux(MM_TAGS[0].as_bytes(), mm)
        .expect("failed to add MM tag");
    record
        .push_aux(ML_TAGS[0].as_bytes(), ml)
        .expect("failed to add ML tag");

    Ok(record)
}

impl Update {
    fn run(&self) -> Result<(), String> {
        let _handle = init_logging(self.log_filepath.as_ref());
        let fp = &self.in_bam;
        let out_fp = &self.out_bam;
        let threads = self.threads;
        let mut reader =
            bam::Reader::from_path(fp).map_err(|e| e.to_string())?;
        reader.set_threads(threads).map_err(|e| e.to_string())?;
        let mut header = bam::Header::from_template(reader.header());
        add_modkit_pg_records(&mut header);

        let mut out_bam =
            bam::Writer::from_path(out_fp, &header, bam::Format::Bam)
                .map_err(|e| e.to_string())?;
        let spinner = get_spinner();

        spinner.set_message("Updating ModBAM");
        let mut total = 0usize;
        let mut total_failed = 0usize;
        let mut total_skipped = 0usize;

        for (i, result) in reader.records().enumerate() {
            if let Ok(record) = result {
                let record_name = util::get_query_name_string(&record)
                    .unwrap_or("???".to_owned());
                match update_mod_tags(
                    record,
                    self.mode.map(|m| m.to_skip_mode()),
                ) {
                    Err(RunError::BadInput(InputError(err)))
                    | Err(RunError::Failed(err)) => {
                        debug!("read {} failed, {}", record_name, err);
                        total_failed += 1;
                    }
                    Err(RunError::Skipped(_reason)) => {
                        total_skipped += 1;
                    }
                    Ok(record) => {
                        if let Err(err) = out_bam.write(&record) {
                            debug!("failed to write {}", err);
                            total_failed += 1;
                        } else {
                            spinner.inc(1);
                            total = i;
                        }
                    }
                }
            } else {
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
