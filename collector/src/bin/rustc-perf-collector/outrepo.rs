//! Write benchmark information to the output repository

use std::fs::{self, read_dir, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::str;
use std::collections::HashSet;

use std::thread;
use std::time::{self, Instant};
use serde_json;
use collector::CommitData;
use chrono::{Duration, Utc};
use rust_sysroot::git::Commit as GitCommit;
use execute::Benchmark;
use failure::{Error, ResultExt};

pub struct Repo {
    path: PathBuf,
    use_remote: bool,
    retries: Vec<String>,
}

impl Repo {
    fn git(&self, args: &[&str]) -> Result<(), Error> {
        for iteration in 0..5 {
            let mut command = Command::new("git");
            command.current_dir(&self.path);
            info!("[{}/5]: git {:?}", iteration, args);
            command.args(args);
            let mut child = command
                .spawn()
                .with_context(|_| format!("could not spawn git {:?}", args))?;
            let start_time = Instant::now();
            loop {
                if start_time.elapsed().as_secs() > 3 {
                    warn!("killing git command -- timed out");
                    child.kill()?;
                    break;
                }
                match child.try_wait() {
                    Ok(Some(status)) => {
                        if status.success() {
                            return Ok(());
                        } else {
                            bail!(
                                "command `git {:?}` failed in `{}`",
                                args,
                                self.path.display()
                            );
                        }
                    }
                    Ok(None) => thread::sleep(time::Duration::from_millis(250)),
                    Err(err) => bail!("command `git {:?}` failed to try_wait in {:?}: {:?}",
                        args, self.path.display(), err),

                }
            }
        }
        bail!("failed to run git command, timed out too many times")
    }

    pub fn open(path: PathBuf, allow_new_dir: bool, use_remote: bool) -> Result<Self, Error> {
        let mut result = Repo {
            path: path,
            use_remote,
            retries: vec![],
        };

        // Don't nuke random repositories, unless specifically requested.
        if !allow_new_dir && !result.retries_file().exists() {
            bail!("`{}` file not present", result.retries_file().display());
        }

        if result.use_remote {
            result.git(&["fetch"])?;
            result.git(&["reset", "--hard", "@{upstream}"])?;
        }

        fs::create_dir_all(result.times()).context("can't create `times/`")?;
        result.load_retries()?;

        Ok(result)
    }

    pub fn success(&self, data: &CommitData) -> Result<(), Error> {
        self.add_commit_data(data)?;
        self.commit_and_push(&format!("{} - success", data.commit.sha))?;
        Ok(())
    }

    pub fn find_missing_commits<'a>(
        &self,
        commits: &'a [GitCommit],
        benchmarks: &[Benchmark],
        triple: &str,
    ) -> Result<Vec<&'a GitCommit>, Error> {
        let mut have = HashSet::new();
        let path = self.times();
        for entry in read_dir(path)? {
            let entry = entry?;
            let filename = entry.file_name().to_string_lossy().to_string();
            let sha =
                &filename[filename.find("00:00").unwrap() + 6..filename.find("-x86").unwrap()];
            have.insert(sha.to_string());
        }

        if let Ok(file) = File::open(self.broken_commits_file()) {
            let file = BufReader::new(file);
            for line in file.lines() {
                let line = line?;
                let sha = &line[..line.find(":").unwrap()];
                have.insert(sha.to_string());
            }
        }

        let missing = commits
            .iter()
            .filter(|c| Utc::now().signed_duration_since(c.date) < Duration::days(29))
            .filter(|c| {
                !have.contains(&c.sha) || {
                    self.load_commit_data(c, triple)
                        .ok()
                        .map(|data| {
                            benchmarks
                                .iter()
                                .any(|b| data.benchmarks.keys().find(|k| **k == b.name).is_none())
                        })
                        .unwrap_or(true)
                }
            })
            .collect::<Vec<_>>();

        Ok(missing)
    }

    fn commit_and_push(&self, message: &str) -> Result<(), Error> {
        self.write_retries()?;
        self.git(&["add", "retries", "times"])?;

        // dirty index
        if let Err(_) = self.git(&["diff-index", "--quiet", "--cached", "HEAD"]) {
            self.git(&["commit", "-m", message])?;
            if self.use_remote {
                self.git(&["push"])?;
            }
        } else {
            println!("nothing to commit...");
        }
        Ok(())
    }

    pub fn load_commit_data(&self, commit: &GitCommit, triple: &str) -> Result<CommitData, Error> {
        let filepath = self.times().join(format!(
            "{}-{}-{}.json",
            commit.date.to_rfc3339(),
            commit.sha,
            triple
        ));
        trace!("loading file {}", filepath.display());
        let mut file = File::open(&filepath)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        let data = serde_json::from_str(&contents)
            .with_context(|_| format!("failed to read JSON from {:?}", filepath))?;
        Ok(data)
    }

    pub fn add_commit_data(&self, data: &CommitData) -> Result<(), Error> {
        let commit = &data.commit;
        let filepath = self.times().join(format!(
            "{}-{}-{}.json",
            commit.date, commit.sha, data.triple
        ));
        info!("creating file {}", filepath.display());
        let mut file = File::create(&filepath)?;
        serde_json::to_writer(&mut file, &data)?;
        Ok(())
    }

    fn load_retries(&mut self) -> Result<(), Error> {
        let mut retries = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(self.retries_file())
            .with_context(|_| format!("can't create `{}`", self.retries_file().display()))?;
        let mut retries_s = String::new();
        retries.read_to_string(&mut retries_s)?;
        self.retries = retries_s
            .split('\n')
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .map(|line| {
                if line.len() == 40 {
                    Ok(line.to_owned())
                } else {
                    bail!("bad retry hash `{}`", line)
                }
            })
            .collect::<Result<_, _>>()?;
        info!("loaded retries: {:?}", self.retries);
        Ok(())
    }

    fn write_retries(&self) -> Result<(), Error> {
        info!("writing retries");
        let mut retries = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(self.retries_file())
            .context("can't create `retries`")?;
        for retry in self.retries.iter() {
            writeln!(retries, "{}", retry)?;
        }
        Ok(())
    }

    pub fn write_broken_commit(&self, commit: &GitCommit, err: Error) -> Result<(), Error> {
        info!("writing broken commit {:?}: {:?}", commit, err);
        let mut broken = OpenOptions::new()
            .write(true)
            .append(true)
            .create(true)
            .open(self.broken_commits_file())
            .context("can't create `broken-commits-log`")?;
        writeln!(broken, "{}: \"{:?}\"", commit.sha, err)?;
        Ok(())
    }

    pub fn next_retry(&mut self) -> Option<String> {
        if self.retries.len() == 0 {
            None
        } else {
            Some(self.retries.remove(0))
        }
    }

    fn broken_commits_file(&self) -> PathBuf {
        self.path.join("broken-commits-log")
    }

    fn retries_file(&self) -> PathBuf {
        self.path.join("retries")
    }

    fn times(&self) -> PathBuf {
        self.path.join("times")
    }
}
