use crate::mod_bam::{
    filter_records_iter, BaseModCall, BaseModProbs, CollapseMethod,
    ModBaseInfo, TrackingModRecordIter,
};
use crate::mod_base_code::DnaBase;
use crate::monoid::Moniod;
use anyhow::anyhow;
use bio::alphabets::dna::{complement, revcomp};
use derive_new::new;
use indicatif::ParallelProgressIterator;
use itertools::Itertools;
use log::{debug, error};
use rust_htslib::bam::{self, Read, Records};
use std::collections::{HashMap, HashSet};

use crate::errs::RunError;
use crate::reads_sampler::record_sampler::{Indicator, RecordSampler};
use crate::record_processor::{RecordProcessor, WithRecords};
use crate::util::{
    get_master_progress_bar, get_query_name_string, get_spinner, Strand,
};
use rayon::prelude::*;
use rust_htslib::bam::ext::BamRecordExtensions;
use rust_htslib::bam::record::Cigar;

/// Read IDs mapped to their base modification probabilities, organized
/// by the canonical base. This data structure contains essentially all
/// of the same data as in the records themselves, but with the query
/// position and the alternative probabilities removed (i.e. it only has
/// the probability of the called modification).
pub(crate) struct ReadIdsToBaseModProbs {
    // mapping of read id to canonical base mapped to a vec
    // of base mod calls on that canonical base
    pub(crate) inner: HashMap<String, HashMap<DnaBase, Vec<BaseModProbs>>>,
}

impl ReadIdsToBaseModProbs {
    fn add_read_without_probs(&mut self, read_id: &str) {
        self.inner
            .entry(read_id.to_owned())
            .or_insert(HashMap::new());
    }

    fn add_mod_probs_for_read(
        &mut self,
        read_id: &str,
        canonical_base: DnaBase,
        mod_probs: Vec<BaseModProbs>,
    ) {
        let added = self
            .inner
            .entry(read_id.to_owned())
            .or_insert(HashMap::new())
            .insert(canonical_base, mod_probs);
        if added.is_some() {
            debug!(
                "double added base mod calls for base {} and read {},\
                 potentially a logic error, please submit an issue.",
                canonical_base.char(),
                read_id
            );
        }
    }

    #[inline]
    /// Returns most likely probabilities for base modifications predicted for
    /// each canonical base.
    pub(crate) fn mle_probs_per_base(&self) -> HashMap<DnaBase, Vec<f32>> {
        let pb = get_master_progress_bar(self.inner.len());
        pb.set_message("aggregating per-base modification probabilities");
        self.inner
            .par_iter()
            .progress_with(pb)
            .map(|(_, can_base_to_base_mod_probs)| {
                can_base_to_base_mod_probs
                    .iter()
                    .map(|(canonical_base, base_mod_probs)| {
                        let probs = base_mod_probs
                            .iter()
                            .filter_map(|bmc| match bmc.argmax_base_mod_call() {
                                Ok(BaseModCall::Modified(f, _)) => Some(f),
                                Ok(BaseModCall::Canonical(f)) => Some(f),
                                Ok(BaseModCall::Filtered) => {
                                    unreachable!(
                                        "argmax base mod call should not return Filtered"
                                    )
                                }
                                Err(e) => {
                                    debug!("{}", e.to_string());
                                    None
                                }
                            })
                            .collect::<Vec<f32>>();
                        (*canonical_base, probs)
                    })
                    .collect::<HashMap<DnaBase, Vec<f32>>>()
            })
            .reduce(|| HashMap::zero(), |a, b| a.op(b))
    }

    /// return argmax probs for each mod-code
    pub(crate) fn mle_probs_per_base_mod(&self) -> HashMap<char, Vec<f64>> {
        // todo(arand) should really aggregate per mod-code
        let pb = get_master_progress_bar(self.inner.len());
        pb.set_message("aggregating per-mod probabilities");
        self.inner
            .par_iter()
            .progress_with(pb)
            .filter_map(|(_, base_mod_probs)| {
                let grouped = base_mod_probs
                    .iter()
                    .map(|(base, base_mod_probs)| {
                        base_mod_probs
                            .iter()
                            // can make this .base_mod_call
                            .filter_map(|bmc| match bmc.argmax_base_mod_call() {
                                Ok(BaseModCall::Modified(p, code)) => {
                                    Some((code.char(), p as f64))
                                }
                                Ok(BaseModCall::Canonical(p)) => {
                                    Some((base.char(), p as f64))
                                }
                                Ok(BaseModCall::Filtered) => {
                                    unreachable!(
                                        "argmax base mod call should not return Filtered"
                                    )
                                }
                                Err(e) => {
                                    debug!("{}", e.to_string());
                                    None
                                }
                            })
                            .fold(
                                HashMap::<char, Vec<f64>>::new(),
                                |mut acc, (base, p)| {
                                    acc.entry(base).or_insert(Vec::new()).push(p);
                                    acc
                                },
                            )
                    })
                    .reduce(|a, b| a.op(b));
                grouped
            })
            .reduce(|| HashMap::zero(), |a, b| a.op(b))
    }

    pub(crate) fn seen(&self, record_name: &str) -> bool {
        self.inner.contains_key(record_name)
    }
}

impl Moniod for ReadIdsToBaseModProbs {
    fn zero() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    fn op(self, other: Self) -> Self {
        let mut acc = self.inner;
        for (read_id, base_mod_calls) in other.inner {
            if acc.contains_key(&read_id) {
                continue;
            } else {
                acc.insert(read_id, base_mod_calls);
            }
        }

        Self { inner: acc }
    }

    fn op_mut(&mut self, other: Self) {
        for (read_id, base_mod_calls) in other.inner {
            if self.inner.contains_key(&read_id) {
                continue;
            } else {
                self.inner.insert(read_id, base_mod_calls);
            }
        }
    }

    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl RecordProcessor for ReadIdsToBaseModProbs {
    type Output = Self;

    fn process_records<T: Read>(
        records: Records<T>,
        with_progress: bool,
        mut record_sampler: RecordSampler,
        collapse_method: Option<&CollapseMethod>,
    ) -> anyhow::Result<Self::Output> {
        let spinner = if with_progress {
            Some(record_sampler.get_progress_bar())
        } else {
            None
        };
        let mod_base_info_iter = filter_records_iter(records);
        let mut read_ids_to_mod_base_probs = Self::zero();
        for (record, mod_base_info) in mod_base_info_iter {
            match record_sampler.ask() {
                Indicator::Use(token) => {
                    let record_name = get_query_name_string(&record);
                    if record_name.is_err() {
                        debug!("record name failed UTF-8 decode");
                        continue;
                    }
                    let record_name = record_name.unwrap();
                    if read_ids_to_mod_base_probs.seen(&record_name) {
                        debug!(
                            "already processed {record_name}, consider de-duplicating alignments.");
                        continue;
                    }

                    if mod_base_info.is_empty() {
                        // add count of unused/no calls
                        read_ids_to_mod_base_probs
                            .add_read_without_probs(&record_name);
                        continue;
                    }

                    let (_, base_mod_probs_iter) =
                        mod_base_info.into_iter_base_mod_probs();
                    for (raw_canonical_base, strand, seq_pos_base_mod_probs) in
                        base_mod_probs_iter
                    {
                        let canonical_base = match (
                            DnaBase::parse(raw_canonical_base),
                            strand,
                        ) {
                            (Err(_), _) => continue,
                            (Ok(dna_base), Strand::Positive) => dna_base,
                            (Ok(dna_base), Strand::Negative) => {
                                dna_base.complement()
                            }
                        };
                        let mod_probs = seq_pos_base_mod_probs
                            .pos_to_base_mod_probs
                            .into_iter()
                            .map(|(_q_pos, base_mod_probs)| {
                                if let Some(method) = collapse_method {
                                    base_mod_probs.into_collapsed(method)
                                } else {
                                    base_mod_probs
                                }
                            })
                            .collect::<Vec<BaseModProbs>>();
                        read_ids_to_mod_base_probs.add_mod_probs_for_read(
                            &record_name,
                            canonical_base,
                            mod_probs,
                        );
                    }
                    if let Some(pb) = &spinner {
                        pb.inc(1);
                    }
                    record_sampler.used(token);
                }
                Indicator::Skip => continue,
                Indicator::Done => break,
            }
        }

        if let Some(pb) = &spinner {
            pb.finish_and_clear();
        }

        Ok(read_ids_to_mod_base_probs)
    }
}

impl WithRecords for ReadIdsToBaseModProbs {
    fn size(&self) -> u64 {
        let s = self
            .inner
            .iter()
            .map(|(_, base_mod_calls)| {
                base_mod_calls.values().map(|vs| vs.len()).sum::<usize>()
            })
            .sum::<usize>();
        s as u64
    }

    fn num_reads(&self) -> usize {
        self.inner.len()
    }
}

#[derive(new, Debug)]
pub(crate) struct ModProfile {
    query_position: usize,
    pub(crate) ref_position: Option<i64>,
    num_soft_clipped_start: usize,
    num_soft_clipped_end: usize,
    read_length: usize,
    q_mod: f32,
    raw_mod_code: char,
    q_base: u8,
    query_kmer: [u8; 5],
    mod_strand: Strand,
    pub(crate) alignment_strand: Option<Strand>,
    canonical_base: char,
}

impl ModProfile {
    pub(crate) fn header() -> String {
        let tab = '\t';
        format!(
            "\
            read_id{tab}\
            forward_read_position{tab}\
            ref_position{tab}\
            chrom{tab}\
            mod_strand{tab}\
            ref_strand{tab}\
            fw_soft_clipped_start{tab}\
            fw_soft_clipped_end{tab}\
            read_length{tab}\
            mod_qual{tab}\
            mod_code{tab}\
            base_qual{tab}\
            ref_kmer{tab}\
            query_kmer{tab}\
            canonical_base"
        )
    }

    pub(crate) fn to_row(
        &self,
        read_id: &str,
        chrom_name: &str,
        reference_seqs: &HashMap<String, Vec<u8>>,
    ) -> String {
        let query_kmer = self.query_kmer.iter().map(|c| *c as char).join("");
        let ref_kmer = if let Some(ref_pos) = self.ref_position {
            if ref_pos < 0 {
                ".".to_string()
            } else {
                reference_seqs
                    .get(chrom_name)
                    .map(|s| {
                        ReadsBaseModProfile::get_fivemer(s, ref_pos as usize)
                            .iter()
                            .map(|b| *b as char)
                            .join("")
                    })
                    .unwrap_or(".".to_string())
            }
        } else {
            ".".to_string()
        };
        let sep = '\t';

        format!(
            "\
            {read_id}{sep}\
            {}{sep}\
            {}{sep}\
            {chrom_name}{sep}\
            {}{sep}\
            {}{sep}\
            {}{sep}\
            {}{sep}\
            {}{sep}\
            {}{sep}\
            {}{sep}\
            {}{sep}\
            {}{sep}\
            {}{sep}\
            {}\n",
            self.query_position,
            self.ref_position.unwrap_or(-1),
            self.mod_strand.to_char(),
            self.alignment_strand.map(|s| s.to_char()).unwrap_or('.'),
            self.num_soft_clipped_start,
            self.num_soft_clipped_end,
            self.read_length,
            self.q_mod,
            self.raw_mod_code,
            self.q_base,
            ref_kmer,
            query_kmer,
            self.canonical_base
        )
    }
}

#[derive(new, Debug)]
pub(crate) struct ReadBaseModProfile {
    pub(crate) record_name: String,
    pub(crate) chrom_id: Option<u32>,
    pub(crate) profile: Vec<ModProfile>,
}

impl ReadBaseModProfile {
    pub(crate) fn process_record(
        record: &bam::Record,
        record_name: &str,
        mod_base_info: ModBaseInfo,
        collapse_method: Option<&CollapseMethod>,
    ) -> Result<Self, RunError> {
        let read_length = record.seq_len();
        let (num_clip_start, num_clip_end) =
            match ReadsBaseModProfile::get_soft_clipped(
                record.cigar().as_slice(),
            ) {
                Ok((sc_start, sc_end)) => {
                    if record.is_reverse() {
                        (sc_end, sc_start)
                    } else {
                        (sc_start, sc_end)
                    }
                }
                Err(e) => {
                    debug!(
                        "record {record_name} has improper CIGAR, {}",
                        e.to_string()
                    );
                    return Err(RunError::new_failed(
                        "inproper CIGAR".to_string(),
                    ));
                }
            };
        let (alignment_strand, chrom_tid) = if record.is_unmapped() {
            (None, None)
        } else {
            if record.is_reverse() {
                (Some(Strand::Negative), Some(record.tid() as u32))
            } else {
                (Some(Strand::Positive), Some(record.tid() as u32))
            }
        };
        // mapping of _forward_ query position to (alignment_qpos, ref_pos)
        let forward_query_pos_to_ref_pos = if record.is_unmapped() {
            HashMap::new()
        } else {
            record
                .aligned_pairs_full()
                .filter_map(|pair| {
                    match (pair[0], pair[1]) {
                        // aligned
                        (Some(qpos), r_pos) => {
                            if qpos < 0 {
                                None
                            } else {
                                let qpos = qpos as usize;
                                if record.is_reverse() {
                                    read_length
                                        .checked_sub(qpos as usize + 1)
                                        // todo make sure you dont need to check that r_pos is < 0
                                        .map(|qpos_adj| {
                                            (qpos_adj, (qpos, r_pos))
                                        })
                                } else {
                                    Some((qpos as usize, (qpos, r_pos)))
                                }
                            }
                        }
                        // delete
                        (None, _) => None,
                    }
                })
                .collect::<HashMap<usize, (usize, Option<i64>)>>()
        };
        let (_, mod_probs_iter) = mod_base_info.into_iter_base_mod_probs();
        let quals = if record.is_reverse() {
            record.qual().to_vec().into_iter().rev().collect()
        } else {
            record.qual().to_vec()
        };
        let mut mod_profiles = mod_probs_iter
            .flat_map(|(primary_base, mod_strand, seq_pos_base_mod_probs)| {
                seq_pos_base_mod_probs
                    .pos_to_base_mod_probs
                    .into_par_iter()
                    .filter_map(|(q_pos_forward, probs)| {
                        let probs = if let Some(method) = collapse_method {
                            probs.into_collapsed(method)
                        } else {
                            probs
                        };
                        let base_qual = match quals.get(q_pos_forward) {
                            Some(q) => *q,
                            None => {
                                error!("didn't find qual!");
                                0u8
                            }
                        };
                        let fivemer = ReadsBaseModProfile::get_fivemer_from_seq(
                            &record,
                            q_pos_forward,
                            mod_strand,
                        );
                        if let Some((_q_pos_aln, ref_pos)) =
                            forward_query_pos_to_ref_pos.get(&q_pos_forward)
                        {
                            let profiles = probs
                                .iter_probs()
                                .map(|(raw_mod_code, prob)| {
                                    ModProfile::new(
                                        q_pos_forward,
                                        *ref_pos,
                                        num_clip_start,
                                        num_clip_end,
                                        read_length,
                                        *prob,
                                        *raw_mod_code,
                                        base_qual,
                                        fivemer,
                                        mod_strand,
                                        alignment_strand,
                                        primary_base,
                                    )
                                })
                                .collect::<Vec<ModProfile>>();
                            Some(profiles)
                        } else {
                            let profiles = probs
                                .iter_probs()
                                .map(|(raw_mod_code, prob)| {
                                    ModProfile::new(
                                        q_pos_forward,
                                        None,
                                        num_clip_start,
                                        num_clip_end,
                                        read_length,
                                        *prob,
                                        *raw_mod_code,
                                        base_qual,
                                        fivemer,
                                        mod_strand,
                                        alignment_strand,
                                        primary_base,
                                    )
                                })
                                .collect::<Vec<ModProfile>>();
                            Some(profiles)
                        }
                    })
                    .flatten()
                    .collect::<Vec<ModProfile>>()
            })
            .collect::<Vec<ModProfile>>();
        mod_profiles.par_sort_by(|a, b| {
            if record.is_reverse() {
                b.query_position.cmp(&a.query_position)
            } else {
                a.query_position.cmp(&b.query_position)
            }
        });

        Ok(Self {
            record_name: record_name.to_owned(),
            chrom_id: chrom_tid,
            profile: mod_profiles,
        })
    }
}

#[derive(new, Debug)]
pub(crate) struct ReadsBaseModProfile {
    pub(crate) profiles: Vec<ReadBaseModProfile>,
    pub(crate) num_skips: usize,
    pub(crate) num_fails: usize,
}

impl ReadsBaseModProfile {
    fn get_fivemer(seq: &[u8], pos: usize) -> [u8; 5] {
        let missing = 45u8;
        let fivemer = [
            seq.get(pos - 2).map(|b| *b),
            seq.get(pos - 1).map(|b| *b),
            seq.get(pos).map(|b| *b),
            seq.get(pos + 1).map(|b| *b),
            seq.get(pos + 2).map(|b| *b),
        ];
        fivemer.map(|b| b.unwrap_or(missing))
    }

    fn get_fivemer_from_seq(
        record: &bam::Record,
        forward_position: usize,
        mod_strand: Strand,
    ) -> [u8; 5] {
        let seq = if record.is_reverse() {
            revcomp(record.seq().as_bytes())
        } else {
            record.seq().as_bytes()
        };
        let missing = 45;
        let get_back_base_safe = |i| -> Option<u8> {
            forward_position
                .checked_sub(i)
                .and_then(|idx| seq.get(idx).map(|b| *b))
        };
        let fivemer = [
            get_back_base_safe(2),
            get_back_base_safe(1),
            seq.get(forward_position).map(|b| *b),
            seq.get(forward_position + 1).map(|b| *b),
            seq.get(forward_position + 2).map(|b| *b),
        ];

        let fivemer = match mod_strand {
            Strand::Positive => fivemer,
            Strand::Negative => {
                let mut comp = fivemer.map(|b| b.map(complement));
                comp.reverse();
                comp
            }
        };

        fivemer.map(|b| b.unwrap_or(missing))
    }

    fn get_soft_clipped(cigar: &[Cigar]) -> anyhow::Result<(usize, usize)> {
        let mut sc_start = None;
        let mut sc_end = None;
        for op in cigar {
            match op {
                Cigar::SoftClip(l) => match (sc_start, sc_end) {
                    (None, None) => sc_start = Some(*l as usize),
                    (Some(_), None) => {
                        sc_end = Some(*l as usize);
                    }
                    (Some(_), Some(_)) => {
                        return Err(anyhow!(
                            "encountered softclip operation more than twice"
                        ));
                    }
                    (None, Some(_)) => unreachable!("logic error"),
                },
                _ => {}
            }
        }
        Ok((sc_start.unwrap_or(0), sc_end.unwrap_or(0)))
    }
}

impl Moniod for ReadsBaseModProfile {
    fn zero() -> Self {
        Self {
            profiles: Vec::new(),
            num_skips: 0,
            num_fails: 0,
        }
    }

    fn op(self, other: Self) -> Self {
        let seen = self
            .profiles
            .iter()
            .map(|p| p.record_name.as_str())
            .collect::<HashSet<&str>>();
        let to_add = other
            .profiles
            .into_iter()
            .filter(|p| !seen.contains(p.record_name.as_str()))
            .collect::<Vec<ReadBaseModProfile>>();
        drop(seen);
        let mut profiles = self.profiles;
        profiles.extend(to_add.into_iter());

        let num_skips = self.num_skips + other.num_skips;
        let num_fails = self.num_fails + other.num_fails;
        Self {
            profiles,
            num_skips,
            num_fails,
        }
    }

    fn op_mut(&mut self, other: Self) {
        let seen = self
            .profiles
            .iter()
            .map(|p| p.record_name.as_str())
            .collect::<HashSet<&str>>();
        let to_add = other
            .profiles
            .into_iter()
            .filter(|p| !seen.contains(p.record_name.as_str()))
            .collect::<Vec<ReadBaseModProfile>>();
        drop(seen);
        self.profiles.extend(to_add.into_iter());

        self.num_skips += other.num_skips;
        self.num_fails += other.num_fails;
    }

    fn len(&self) -> usize {
        self.profiles.len()
    }
}

impl RecordProcessor for ReadsBaseModProfile {
    type Output = Self;

    fn process_records<T: Read>(
        records: Records<T>,
        with_progress: bool,
        mut record_sampler: RecordSampler,
        collapse_method: Option<&CollapseMethod>,
    ) -> anyhow::Result<Self::Output> {
        let mut mod_iter = TrackingModRecordIter::new(records);
        let mut agg = Vec::new();
        let mut seen = HashSet::new();
        let pb = if with_progress {
            Some(get_spinner())
        } else {
            None
        };

        let mut n_fails = 0usize;
        let mut n_skips = 0usize;
        for (record, record_name, modbase_info) in &mut mod_iter {
            match record_sampler.ask() {
                Indicator::Use(token) => {
                    match ReadBaseModProfile::process_record(
                        &record,
                        &record_name,
                        modbase_info,
                        collapse_method,
                    ) {
                        Ok(read_base_mod_profile) => {
                            if seen.contains(&record_name) {
                                debug!("double add of record {record_name}");
                            } else {
                                seen.insert(record_name);
                            }
                            agg.push(read_base_mod_profile);

                            if let Some(pb) = &pb {
                                pb.inc(1);
                            }
                            record_sampler.used(token);
                        }
                        Err(run_error) => match run_error {
                            RunError::Failed(_) | RunError::BadInput(_) => {
                                n_fails += 1;
                            }
                            RunError::Skipped(_) => n_skips += 1,
                        },
                    }
                }
                Indicator::Skip => continue,
                Indicator::Done => break,
            }
        }

        let num_failed = mod_iter.num_failed + n_fails;
        let num_skipped = mod_iter.num_skipped + n_skips;

        Ok(ReadsBaseModProfile {
            profiles: agg,
            num_skips: num_skipped,
            num_fails: num_failed,
        })
    }
}

impl WithRecords for ReadsBaseModProfile {
    fn size(&self) -> u64 {
        self.profiles
            .iter()
            .map(|p| p.profile.len() as u64)
            .sum::<u64>()
    }

    fn num_reads(&self) -> usize {
        self.profiles.len()
    }
}

#[cfg(test)]
mod read_ids_to_base_mod_probs_tests {
    #[test]
    fn test_cigar_finds_softclips() {
        // todo
    }
}