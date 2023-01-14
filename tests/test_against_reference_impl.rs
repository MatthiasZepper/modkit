use rust_htslib::bam;
use rust_htslib::bam::Read;
use std::io::Read as StdRead;
use std::path::PathBuf;
use std::process::Output;

#[test]
fn test_help() {
    let exe = std::path::Path::new(env!("CARGO_BIN_EXE_modkit"));
    assert!(exe.exists());

    let help = std::process::Command::new(exe)
        .arg("collapse")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert!(help.status.success());
}

fn run_collapse(args: &[&str]) -> Output {
    let exe = std::path::Path::new(env!("CARGO_BIN_EXE_modkit"));
    assert!(exe.exists());

    let output = std::process::Command::new(exe)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert!(output.status.success());
    output
}

fn test_collapse_output(
    input_path: &str,
    output_path: &str,
    check_file_path: &str,
) {
    let temp_file = std::env::temp_dir().join(output_path);
    let args = ["collapse", input_path, temp_file.to_str().unwrap()];
    run_collapse(&args);
    assert!(temp_file.exists());

    let mut test_bam = bam::Reader::from_path(temp_file).unwrap();
    let mut ref_bam = bam::Reader::from_path(check_file_path).unwrap();
    for (test_res, ref_res) in test_bam.records().zip(ref_bam.records()) {
        let test_record = test_res.unwrap();
        let ref_record = ref_res.unwrap();
        assert_eq!(ref_record, test_record);
    }
}

#[test]
fn test_collapse_canonical() {
    test_collapse_output(
        "tests/resources/input_C.bam",
        "test_C.bam",
        "tests/resources/ref_out_C_auto.bam",
    );
}

#[test]
fn test_collapse_methyl() {
    test_collapse_output(
        "tests/resources/input_5mC.bam",
        "test_5mC.bam",
        "tests/resources/ref_out_5mC_auto.bam",
    );
}

#[test]
fn test_collapse_no_tags() {
    let temp_file = std::env::temp_dir().join("test_out_no_tags.bam");
    run_collapse(&[
        "collapse",
        "tests/resources/input_C_no_tags.bam",
        temp_file.to_str().unwrap(),
    ]);
}

fn check_against_expected_text_file(output_fp: &str, expected_fp: &str) {
    let test = {
        let mut fh = std::fs::File::open(output_fp).unwrap();
        let mut buff = String::new();
        fh.read_to_string(&mut buff).unwrap();
        buff
    };
    let expected = {
        // this file was hand-checked for correctness.
        let mut fh = std::fs::File::open(expected_fp).unwrap();
        let mut buff = String::new();
        fh.read_to_string(&mut buff).unwrap();
        buff
    };

    similar_asserts::assert_eq!(test, expected);
}

fn run_modkit_pileup(args: &[&str]) {
    let exe = std::path::Path::new(env!("CARGO_BIN_EXE_modkit"));
    assert!(exe.exists());

    let output = std::process::Command::new(exe)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert!(output.status.success());
}

#[test]
fn test_mod_pileup_no_filt() {
    let temp_file = std::env::temp_dir().join("test_pileup_nofilt.bed");
    let exe = std::path::Path::new(env!("CARGO_BIN_EXE_modkit"));
    assert!(exe.exists());

    let args = [
        "pileup",
        "-i",
        "25", // use small interval to make sure chunking works
        "--no-filtering",
        "tests/resources/bc_anchored_10_reads.sorted.bam",
        temp_file.to_str().unwrap(),
    ];

    run_modkit_pileup(&args);

    check_against_expected_text_file(
        temp_file.to_str().unwrap(),
        "tests/resources/modbam.modpileup_nofilt.bed",
    );
}

#[test]
fn test_mod_pileup_with_filt() {
    let temp_file = std::env::temp_dir().join("test_pileup_withfilt.bed");
    let exe = std::path::Path::new(env!("CARGO_BIN_EXE_modkit"));
    assert!(exe.exists());

    let args = [
        "pileup",
        "-i",
        "25", // use small interval to make sure chunking works
        "-f",
        "1.0",
        "-p",
        "0.25",
        "--seed",
        "42",
        "tests/resources/bc_anchored_10_reads.sorted.bam",
        temp_file.to_str().unwrap(),
    ];

    run_modkit_pileup(&args);

    check_against_expected_text_file(
        temp_file.to_str().unwrap(),
        "tests/resources/modbam.modpileup_filt025.bed",
    );
}
