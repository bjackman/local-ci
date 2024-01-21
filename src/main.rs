// TODO: I don't want to use clap::Parser, I only want to use Clap. But if I
// don't bring the trait into scope then I can't call .parse on my Args type.
use clap::Parser;
use git2;
use std::fmt;

#[derive(Debug)]
struct GitError {
    desc: &'static str,
    repo_path: String,
    source: git2::Error,
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} for repo {}: {}", self.desc, self.repo_path, self.source)
    }
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value_t = {".".to_string()})]
    repo_path: String,
}

fn do_main() -> Result<(), GitError> {
    let args = Args::parse();

    // TODO: Is there a nice way to make these error constructions more concise?
    // Possibly by redesigning the error types?
    let repo = git2::Repository::open(&args.repo_path).map_err(|e| GitError{
        desc: "opening repo", repo_path: args.repo_path.to_string(), source: e,
    })?;
    let _head = repo.head().map_err(|e| GitError{
        desc: "getting HEAD", repo_path: args.repo_path.to_string(), source: e,
    })?;
    return Ok(());
}

fn main() {
    // TODO: I found if I just return a Result from main, it doesn't use Display
    // it just debug-prints the struct. So here I"m just manually printing the
    // Display representation. Is there a smarter way to do this?
    match do_main() {
        Ok(()) => println!("OK!"),
        Err(e) => println!("{}", e),
    };
}