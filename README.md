# Modkit

A bioinformatics tool for working with modified bases from Oxford Nanopore. Specifically for converting modBAM
to bedMethyl files using best practices, but also manipulating modBAM files and generating summary statistics.

## Creating a bedMethyl pileup from a modBam

The most typical use case, take a BAM with modified bases (as MM/ML or Mm/Ml tags) sum the calls from every
read over each genomic position (a pileup). 

```bash
modkit bed path/to/reads.bam output/path/pileup.bed 
```

No reference is required. A single file (description below) with pileup calls will be created. Modification
filtering will be performed for you.

Some typical options:

1. Only emit calls from reference CpG dinucleotides. This option requires a reference sequence in order to
   locate the CpGs.

```bash
modkit bed path/to/reads.bam output/path/pileup.bed --cpg --reference-fasta path/to/reference.fasta
```
2. Combine 5hmC and 5mC calls into a single modified call (makes the resulting pileup directly comparable to
   whole genome bisulfite sequencing).

```bash
modkit bed path/to/reads.bam output/path/pileup.bed \
    --cpg --reference-fasta path/to/reference.fasta --combine-mods
```

this operation can be performed without the reference/CpG constraint.

```bash
modkit bed path/to/reads.bam output/path/pileup.bed --combine-mods
```

3. Combine CpG calls on opposite strands. CpG motifs are reverse complement equivalent, so you may want to
   combine the calls from the positive stand C with the negative strand C (reference G). This operation
   _requires_ that you use the `--cpg` flag and specify a reference sequence.

```bash
modkit bed path/to/reads.bam output/path/pileup.bed \
    --cpg --reference-fasta path/to/reference.fasta --combine-mods --combine-strands
```

## bedMethyl output description

### Some definitions:

**N_mod**: Number of filtered calls that classified a residue as a specific base modification.  For example, if
the base modification is `h` (5hmC) then this number is the number of filtered reads with a 5hmC call aligned
to this position.

**N_canonical**: Number of filtered calls that classified a residue canonical as opposed to modified. The exact
base must be inferred by the modification code. For example, if the modification code is `m` (5mC) then the
canonical base is cytosine. If the modification code is `a`, the canonical base is adenosine.

**N_other_mod**: Number of filtered calls that classified a residue as modified where the canonical base is the
same, but the actual modification is different. For example, for a given cytosine there may be 3 reads with
`h` calls, 1 with a canonical call, and 2 with `m` calls. In the row for `h` N_other_mod would be 2 and in the
`m` row N_other_mod would be 3.

**filtered_coverage**: N_mod + N_other_mod + N_canonical

**N_diff**: Number of reads with a base other than the canonical base for this modification. For example, in a row
for `h` the canonical base is cytosine, if there are 2 reads with C->A substitutions, N_diff will be 2.

**N_delete**: Number of reads with a delete at this position

**N_filtered**: Number of calls where the probability of the call was below the threshold. The threshold can be
set on the command line or computed from the data (usually filtering out the lowest 10th percentile of calls).

**N_nocall**: Number of reads aligned to this position, with the correct canonical base, but without a base
modification call. This can happen, for example, if the model requires a CpG dinucleotide and the read has a
CG->CH substitution.

### Columns in the bedMethy

```yaml
columns:
  - chrom:
      type: str
      description: name of reference sequence from BAM header
  - start_pos:
      type: int
      description: 0-based index of modified base
  - end_pos:
      type: int
      description: start_pos + 1
  - raw_mod_code:
      type: str
      description: single letter code of modified base
  - score:
      type: int
      description: filtered_coverage
  - strand:
      type: str
      description: + for positive strand - for negative strand
  - start_pos:
      type: int
      description: included for compatibility 
  - end_pos:
      type: int
      description: included for compatibility 
  - color:
      type: str
      description: included for compatibility, always 255,0,0
  - filtered_coverage:
      type: int
      description: see definitions
  - percent_modified:
      type: float
      description: N_mod / filtered_coverage
  - N_mod:
      type: int
  - N_canonical:
      type: int
  - N_other_mod:
      type: int
  - N_delete:
      type: int
  - N_filtered:
      type: int
  - N_diff:
      type: int
  - N_nocall:
      type: int
```

