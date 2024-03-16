use std::{
    collections::HashMap,
    io::{Read, Seek, Write},
    rc::Rc,
};

use anyhow::Result;
use git2::Repository;
use inquire::{Confirm, Select};
use regex::Regex;

use serde_json::Value;
use structopt::StructOpt;

#[derive(StructOpt)]
#[structopt(
    name = "tgit",
    about = "A git tool to help you manage your git repository."
)]
struct Options {
    #[structopt(short = "f", long = "from", help = "The from commit hash or tag.")]
    from: Option<String>,
    #[structopt(
        short = "t",
        long = "to",
        default_value = "HEAD",
        help = "The to commit hash or tag."
    )]
    to: String,
    #[structopt(
        short = "p",
        long = "prefix",
        default_value = "v",
        help = "The prefix of the version."
    )]
    prefix: String,
    #[structopt(
        parse(from_os_str),
        default_value = ".",
        help = "The path of the git repository."
    )]
    path: std::path::PathBuf,
    #[structopt(
        short = "r",
        long = "remote",
        default_value = "origin",
        help = "The remote name."
    )]
    remote: String,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
struct Author {
    name: String,
    mail: String,
    username: String,
}

impl Author {
    fn get_display(&self) -> String {
        if self.username.is_empty() {
            self.name.clone()
        } else {
            format!("@{}", self.username)
        }
    }
}

#[derive(Debug, Clone)]
struct Commit {
    hash: String,
    type_: String,
    scope: String,
    description: String,
    is_breaking: bool,
    authors: Vec<Author>,
}

impl Commit {
    fn new(
        hash: String,
        type_: String,
        scope: String,
        description: String,
        is_breaking: bool,
        authors: Vec<Author>,
    ) -> Self {
        Self {
            hash,
            type_,
            scope,
            description,
            is_breaking,
            authors,
        }
    }
}

#[derive(Debug)]
struct ChangelogUnit<'a> {
    from_commit: Rc<git2::Commit<'a>>,
    to_commit: Rc<git2::Commit<'a>>,
    has_breaking: bool,
    commit_map: HashMap<String, Vec<Commit>>,
    contributors: HashMap<String, Author>,
}

impl<'a> ChangelogUnit<'a> {
    fn new(from_commit: Rc<git2::Commit<'a>>, to_commit: Rc<git2::Commit<'a>>) -> Self {
        Self {
            from_commit,
            to_commit,
            has_breaking: false,
            commit_map: HashMap::new(),
            contributors: HashMap::new(),
        }
    }
}

impl<'a> Clone for ChangelogUnit<'a> {
    fn clone(&self) -> Self {
        let from_commit = self.from_commit.clone();
        let to_commit = self.to_commit.clone();
        let has_breaking = self.has_breaking;
        let commit_map = self.commit_map.clone();
        let contributors = self.contributors.clone();

        ChangelogUnit {
            from_commit: Rc::clone(&from_commit),
            to_commit: Rc::clone(&to_commit),
            has_breaking,
            commit_map,
            contributors,
        }
    }
}

fn main() {
    let args = Options::from_args();
    if let Err(err) = tgit(args) {
        eprintln!("Error: {}", err);
        std::process::exit(1);
    }
}

fn tgit(args: Options) -> Result<(), Box<dyn std::error::Error>> {
    let path = args.path.as_path();
    let from = args.from;
    let to = args.to;
    let remote = args.remote;
    let prefix = args.prefix;
    // println!("from: {:?}", from);
    // println!("to: {}", to);
    let repo = git2::Repository::open(path)?;

    if repo.is_empty().unwrap() {
        return Err("The repository is empty.".into());
    }
    if repo.state() != git2::RepositoryState::Clean {
        return Err("The repository is not clean.".into());
    }
    let statuses = repo.statuses(None).unwrap();
    let has_untracked = statuses.iter().any(|entry| {
        entry.status().contains(git2::Status::WT_NEW)
            || entry.status().contains(git2::Status::INDEX_NEW)
    });
    if has_untracked {
        return Err("The repository has untracked files.".into());
    }

    let mut using_emoji = false;

    let tags = list_tags(&repo);
    let (c2t, _) = get_commit_tag_map(&repo, &tags);
    let range = get_range(&repo, from, to, &c2t)?;
    let host_scope_repo = get_host_scope_repo(&repo, remote.as_str());
    let baseurl = host_scope_repo
        .clone()
        .map_or(String::from(""), |(host, scope, repo)| {
            format!("https://{}/{}/{}/commit", host, scope, repo)
        });

    let (host, scope_name, repo_name) =
        host_scope_repo.unwrap_or(("".to_string(), "".to_string(), "".to_string()));

    let mut idx = range.len() - 2;
    let mut from_commit = range[idx].clone();
    let mut to_commit = range[idx + 1].clone();
    let mut changelog_units = Vec::<ChangelogUnit>::new();
    let mut changelog_unit =
        ChangelogUnit::new(Rc::new(from_commit.clone()), Rc::new(to_commit.clone()));
    if host.contains("github") {
        // 如果仓库和 github 有关，则使用 github 的数据，因为 github 拥有用户信息。
        // eg. https://api.github.com/repos/Jannchie/bumpp/commits?per_page=100&page=1&sha=5d8d761ec9554eceb448e3f62f1d9f1d1841a09f
        let mut mail_to_login = HashMap::<String, String>::new();
        // 已经遍历到的 commit 是否已经超过 to_commit
        let mut over = false;
        // 需要 summary
        let mut should_summary = false;
        for page in 1.. {
            // 如果本地安装了 gh，则使用 gh 获取 commit。这样可以不用配置 token。
            let gh = std::process::Command::new("gh")
                .arg("api")
                .arg(format!(
                    "repos/{}/{}/commits?per_page=100&page={}&sha={}",
                    scope_name,
                    repo_name,
                    page,
                    range.last().unwrap().id(),
                ))
                .output()
                .unwrap();

            // TODO: 如果没有安装 gh，则使用 reqwest 获取 commit。

            // stdout to json
            let data: Value =
                serde_json::from_str(String::from_utf8_lossy(&gh.stdout).to_string().as_str())
                    .unwrap();
            let raw_commits = data.as_array().unwrap();
            for raw_commit in raw_commits {
                // 如果需要总结，则需要将当前的 changelog_unit 复制一份推入 changelog_units
                if should_summary {
                    should_summary = false;
                    // 处理作者信息
                    push_changelog_unit(&mut changelog_unit, &mail_to_login, &mut changelog_units);
                    if idx > 0 {
                        idx -= 1;
                        from_commit = range[idx].clone();
                        to_commit = range[idx + 1].clone();
                        changelog_unit =
                            ChangelogUnit::new(Rc::new(from_commit), Rc::new(to_commit));
                    }
                }

                // 如果已经超了范围，则 break
                if over {
                    break;
                }

                // 处理用户信息
                let raw_commit = raw_commit.as_object().unwrap();
                let sha = raw_commit.get("sha").unwrap().as_str().unwrap().to_string();

                // println!("{:?}", changelog_unit.to_commit);
                // 如果当前的 to 是当前的 sha，则下一次遍历前需要 summary.
                if sha == changelog_unit.from_commit.id().to_string() {
                    // println!("summary: {}", sha);
                    should_summary = true;
                }
                // println!("sha: {}", sha);
                if sha == range.first().unwrap().id().to_string() {
                    over = true;
                }

                let commit = raw_commit.get("commit").unwrap().as_object().unwrap();
                let commit_author = commit.get("author").unwrap().as_object().unwrap();
                let commit_committer = commit.get("committer").unwrap().as_object().unwrap();
                let committer_login = match raw_commit.get("committer").unwrap().as_object() {
                    Some(val) => val.get("login").unwrap().as_str().unwrap(),
                    None => "",
                };
                let committer_mail = commit_committer.get("email").unwrap().as_str().unwrap();
                mail_to_login.insert(committer_mail.to_string(), committer_login.to_string());

                let author_name = commit_author.get("name").unwrap().as_str().unwrap();
                let author_mail = commit_author.get("email").unwrap().as_str().unwrap();

                let author_login = match raw_commit.get("author").unwrap().as_object() {
                    Some(val) => val.get("login").unwrap().as_str().unwrap(),
                    None => "",
                };

                mail_to_login.insert(author_mail.to_string(), author_login.to_string());

                let message = commit.get("message").unwrap().as_str().unwrap();
                let mut authors = vec![Author {
                    name: author_name.to_string(),
                    mail: author_mail.to_string(),
                    username: author_login.to_string(),
                }];
                parse_author_from_body(message, &mut authors);

                let (emoji, scope, description, type_, is_breaking) =
                    match parse_first_line(message.lines().next().unwrap()) {
                        Ok(value) => value,
                        Err(_) => continue,
                    };
                if using_emoji == false && !emoji.is_empty() {
                    using_emoji = true;
                }
                let commit = Commit::new(
                    sha.to_string(),
                    type_,
                    scope,
                    description,
                    is_breaking,
                    authors,
                );
                let commits = changelog_unit
                    .commit_map
                    .entry(commit.type_.clone())
                    .or_insert(Vec::new());
                if commit.is_breaking {
                    changelog_unit.has_breaking = true;
                }
                commits.push(commit);
            }
            if raw_commits.len() < 100 {
                break;
            }
            if over {
                break;
            }
        }
        // println!("{:?}", changelog_unit);
        if should_summary {
            push_changelog_unit(&mut changelog_unit, &mail_to_login, &mut changelog_units);
        }
    } else {
        // 使用本地的 git 信息遍历
        let mut revwalk = repo.revwalk().unwrap();
        revwalk.push_range(
            format!(
                "{}..{}",
                changelog_unit.from_commit.id(),
                changelog_unit.to_commit.id()
            )
            .as_str(),
        )?;
        let (_, _, _) = organize_commit(revwalk, &repo);
    }
    let mut changelog_all = "".to_string();
    let mut first_to_name = "".to_string();
    for changelog_unit in changelog_units {
        let prefix = prefix.clone();
        let baseurl = baseurl.clone();
        let (from_name, to_name) = get_name(
            &changelog_unit.from_commit,
            &changelog_unit.to_commit,
            prefix,
            changelog_unit.has_breaking,
            &changelog_unit.commit_map,
            &c2t,
        );
        if first_to_name.is_empty() {
            first_to_name = to_name.clone();
        }
        let changelog = get_changelog_string(
            baseurl,
            from_name,
            to_name,
            changelog_unit.commit_map,
            changelog_unit.contributors,
        );
        changelog_all.push_str("\n");
        changelog_all.push_str(changelog.as_str());
    }

    let should_bump = Confirm::new("Do you want to bump the version?")
        .with_default(true)
        .prompt()?;

    // 更新 Cargo.toml
    // TODO: package.json, pyproject.toml, setup.py, version.go 之类的文件
    if should_bump {
        update_version(path, &first_to_name, &prefix)?;
    }

    let should_commit_and_push = Confirm::new("Do you want to commit and push?")
        .with_default(true)
        .prompt()?;

    if should_commit_and_push {
        let mut add = std::process::Command::new("git");
        add.arg("add").arg(".");
        let output = add.output()?;
        println!("{}", String::from_utf8_lossy(&output.stdout));

        // commit and push
        let mut commit = std::process::Command::new("git");
        if using_emoji {
            commit.arg("commit").arg("-am").arg(format!(
                "{} release: bump version to {}",
                ":bookmark:", first_to_name
            ));
        } else {
            commit
                .arg("commit")
                .arg("-am")
                .arg(format!("release: bump version to {}", first_to_name));
        }

        let output = commit.output()?;
        println!("{}", String::from_utf8_lossy(&output.stdout));

        // create tag
        let mut tag = std::process::Command::new("git");
        tag.arg("tag").arg(first_to_name);
        let output = tag.output()?;
        println!("{}", String::from_utf8_lossy(&output.stdout));

        // push
        let mut push = std::process::Command::new("git");
        push.arg("push");
        push.arg("origin").arg("HEAD").arg("--tags");
        let output = push.output()?;
        println!("{}", String::from_utf8_lossy(&output.stdout));
    }

    let should_print = Confirm::new("Do you want to print the changelog?")
        .with_default(true)
        .prompt()?;
    println!();
    if should_print {
        println!("{}", changelog_all);
    }
    if false {
        // 如果要求生成或更新 changelog file
        generate_or_update_changelog_file(path, changelog_all)?;
    }
    Result::Ok(())
}

fn update_version(
    path: &std::path::Path,
    version: &String,
    prefix: &String,
) -> Result<(), Box<dyn std::error::Error>> {
    let version_without_prefix = version
        .strip_prefix(prefix.as_str())
        .unwrap_or(&version)
        .to_string();
    let cargo_toml_path = path.join("Cargo.toml");
    if cargo_toml_path.exists() {
        // read toml, update version, write toml
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .open(cargo_toml_path.as_path())?;
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        // 使用正则，匹配内容为 version = "0.1.0" 的行。匹配的行不能有任何其他内容。
        let re = Regex::new(r#"(?m)^version = ".*"\n"#).unwrap();
        let new_content = re.replace_all(
            content.as_str(),
            format!("version = \"{}\"\n", version_without_prefix).as_str(),
        );
        file.seek(std::io::SeekFrom::Start(0))?;
        file.write_all(new_content.as_bytes())?;
    }
    Ok(())
}
fn push_changelog_unit<'a>(
    changelog_unit: &mut ChangelogUnit<'a>,
    mail_to_login: &HashMap<String, String>,
    changelog_units: &mut Vec<ChangelogUnit<'a>>,
) {
    for (_, commits) in &changelog_unit.commit_map {
        for commit in commits {
            for author in &commit.authors {
                if changelog_unit
                    .contributors
                    .contains_key(author.mail.as_str())
                {
                    continue;
                }
                let username = mail_to_login.get(author.mail.as_str());
                let username = match username {
                    Some(username) => username.to_string(),
                    None => "".to_string(),
                };
                let author = Author {
                    name: author.name.to_string(),
                    mail: author.mail.to_string(),
                    username,
                };
                changelog_unit
                    .contributors
                    .insert(author.mail.to_string(), author);
            }
        }
    }
    let unit = changelog_unit.clone();
    // println!("push: {:?}", unit);
    changelog_units.push(unit);
}

fn generate_or_update_changelog_file(
    path: &std::path::Path,
    changelog: String,
) -> Result<(), Box<dyn std::error::Error>> {
    // 如果在仓库目录下（path），存在 CHANGELOG.md 文件，则将 changelog 追加到 CHANGELOG.md 的头部。
    let changelog_path = path.join("CHANGELOG.md");
    Ok(if changelog_path.exists() {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .open(changelog_path.as_path())?;
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        content = format!("{}\n{}", changelog, content);
        file.seek(std::io::SeekFrom::Start(0))?;
        file.write_all(content.as_bytes())?;
    } else {
        let mut file = std::fs::File::create(changelog_path.as_path())?;
        file.write_all(changelog.as_bytes())?;
    })
}

fn get_name(
    from_commit: &git2::Commit<'_>,
    to_commit: &git2::Commit<'_>,
    prefix: String,
    has_breaking: bool,
    commit_map: &HashMap<String, Vec<Commit>>,
    c2t: &HashMap<String, String>,
) -> (String, String) {
    let from_tag = c2t.get(from_commit.id().to_string().as_str());
    let to_tag = c2t.get(to_commit.id().to_string().as_str());
    let from_id_7 = from_commit
        .id()
        .to_string()
        .chars()
        .take(7)
        .collect::<String>();
    let to_id_7 = to_commit
        .id()
        .to_string()
        .chars()
        .take(7)
        .collect::<String>();

    let from_name = from_tag.unwrap_or(&from_id_7).to_string();
    let mut to_name = to_tag.unwrap_or(&to_id_7).to_string();

    if to_name != to_id_7 {
        // 如果 to_name 是 tag，则直接返回
        return (from_name, to_name);
    }
    let mut from_version = semver::Version::parse("0.0.0").unwrap();
    if from_name != from_id_7 {
        // 如果 from_name 是 tag，则 from_version = from_name
        from_version =
            semver::Version::parse(from_name.as_str().strip_prefix(prefix.as_str()).unwrap())
                .unwrap();
    }

    let to_version = from_version.clone();
    let mut default_bump_type = "patch";
    let mut start_cursor = 2;
    if has_breaking {
        default_bump_type = "major";
        start_cursor = 0;
    } else if commit_map.get("feat").is_some() {
        default_bump_type = "minor";
        start_cursor = 1;
    }

    // TODO: 考虑 pre-release 和 build metadata
    let mut to_major_version = to_version.clone();
    to_major_version.pre = semver::Prerelease::EMPTY;
    to_major_version.major += 1;
    to_major_version.minor = 0;
    to_major_version.patch = 0;

    let mut to_minor_version = to_version.clone();
    to_minor_version.pre = semver::Prerelease::EMPTY;
    to_minor_version.minor += 1;
    to_minor_version.patch = 0;

    let mut to_patch_version = to_version.clone();
    to_patch_version.pre = semver::Prerelease::EMPTY;
    to_patch_version.patch += 1;

    let major_option = format!("major ({})", to_major_version);
    let minor_option = format!("minor ({})", to_minor_version);
    let patch_option = format!("patch ({})", to_patch_version);

    let ans = Select::new(
        format!("Select the next version. (current version: {})", to_version).as_str(),
        vec![major_option, minor_option, patch_option],
    )
    .with_starting_cursor(start_cursor)
    .prompt();
    let ans = match ans {
        Ok(ans) => ans,
        Err(_) => default_bump_type.to_string(),
    };
    let to_version = match ans {
        _ if ans.starts_with("major") => to_major_version,
        _ if ans.starts_with("minor") => to_minor_version,
        _ if ans.starts_with("patch") => to_patch_version,
        _ => to_version,
    };
    to_name = format!("{}{}", prefix, to_version);
    let to_name = to_name;
    // println!("from: {}", from_name);
    // println!("to: {}", to_name);
    (from_name, to_name)
}

fn from_commit_get_tag(repo: &Repository, commit: &git2::Commit) -> Option<String> {
    let tags = list_tags(repo);
    for tag_name in tags {
        // 获取标签对应的 commit ID
        let reference = repo
            .find_reference(&format!("refs/tags/{}", tag_name))
            .unwrap();
        let tag_commit = reference.peel_to_commit().unwrap();
        if tag_commit.id() == commit.id() {
            return Some(tag_name);
        }
    }
    None
}

fn list_tags(repo: &Repository) -> Vec<String> {
    let tags = repo.tag_names(None).unwrap();
    let re = Regex::new(
        r"^(?P<prefix>v|ver)?(?P<major>0|[1-9]\d*)\.(?P<minor>0|[1-9]\d*)\.(?P<patch>0|[1-9]\d*)(?:-(?P<prerelease>(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?(?:\+(?P<buildmetadata>[0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$"
    ).unwrap();
    let mut tags: Vec<String> = tags
        .into_iter()
        .filter_map(|tag| {
            tag.and_then(|tag| {
                if re.is_match(tag) {
                    Some(tag.to_string())
                } else {
                    None
                }
            })
        })
        .collect();
    tags.reverse();
    tags
}

fn from_tag_get_commit<'a>(repo: &'a Repository, tag: &'a str) -> Option<git2::Commit<'a>> {
    let reference = repo.find_reference(&format!("refs/tags/{}", tag));
    if reference.is_err() {
        return None;
    }
    let reference = reference.unwrap();
    let tag_commit = reference.peel_to_commit();
    if tag_commit.is_err() {
        return None;
    }
    Some(tag_commit.unwrap())
}

fn get_commit_tag_map(
    repo: &Repository,
    tags: &Vec<String>,
) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut c2t = HashMap::<String, String>::new();
    let mut t2c = HashMap::<String, String>::new();
    for tag in tags.iter() {
        let tag = tag.as_str();
        let commit = from_tag_get_commit(&repo, tag);
        if commit.is_none() {
            continue;
        }
        let commit = commit.unwrap();
        c2t.insert(commit.id().to_string(), tag.to_string());
        t2c.insert(tag.to_string(), commit.id().to_string());
    }
    (c2t, t2c)
}

fn parse_git_url(url: &String) -> Option<(&str, &str, &str)> {
    let ssh_re = Regex::new(r"^git@([^:]+):([^/]+)/(.+).git$").unwrap();
    let http_re = Regex::new(r"^https?://([^/]+)/([^/]+)/(.+)$").unwrap();
    if let Some(captures) = ssh_re.captures(url.as_str()) {
        let host = captures.get(1).unwrap().as_str();
        let scope = captures.get(2).unwrap().as_str();
        let repo = captures.get(3).unwrap().as_str();
        Some((host, scope, repo))
    } else if let Some(captures) = http_re.captures(url.as_str()) {
        let host = captures.get(1).unwrap().as_str();
        let scope = captures.get(2).unwrap().as_str();
        let repo = captures.get(3).unwrap().as_str();
        Some((host, scope, repo))
    } else {
        None
    }
}

fn get_changelog_string(
    baseurl: String,
    from_name: String,
    to_name: String,
    commit_map: HashMap<String, Vec<Commit>>,
    contributors: HashMap<String, Author>,
) -> String {
    let types = vec![
        "feat", "feat", "fix", "docs", "style", "refactor", "perf", "test", "build", "ci", "chore",
        "revert", "other",
    ];
    let name_map = vec![
        ":sparkles: Breaking Changes",
        ":sparkles: Features",
        ":bug: Bug Fixes",
        ":memo: Documentation",
        ":art: Styles",
        ":recycle: Code Refactoring",
        ":zap: Performance Improvements",
        ":rotating_light: Tests",
        ":hammer: Build",
        ":green_heart: Continuous Integration",
        ":wrench: Chores",
        ":rewind: Reverts",
        ":package: Others",
    ];
    let baseurl = baseurl;
    let mut changelog = String::new();
    changelog.push_str(format!("## {}\n\n", to_name).as_str());
    let compare_url = format!("/compare/{}...{}", from_name, to_name);
    let url = format!("{}{}", baseurl, compare_url);

    if !baseurl.is_empty() {
        changelog.push_str(format!("[compare changes]({})\n", url).as_str());
    }
    for (i, type_) in types.iter().enumerate() {
        let commits = commit_map.get(*type_);
        let commits = match commits {
            Some(commits) => commits,
            None => continue,
        };
        if commits.is_empty() {
            continue;
        }
        if i == 0 && commits.iter().filter(|commit| commit.is_breaking).count() == 0 {
            continue;
        }
        if i == 1 && commits.iter().filter(|commit| !commit.is_breaking).count() == 0 {
            continue;
        }
        changelog.push_str(format!("\n### {}\n\n", name_map[i]).as_str());
        for commit in commits {
            if i == 0 && !commit.is_breaking || i == 1 && commit.is_breaking {
                continue;
            }
            // 生成 by 信息
            let mut by = String::from("");
            // by 信息的格式类似：by author1, author2, and author3
            for (i, author) in commit.authors.iter().enumerate() {
                let author_display = author.get_display();
                if i == 0 {
                    by.push_str("by ");
                }
                if commit.authors.len() == 1 {
                    by.push_str(format!("{}", author_display).as_str());
                } else {
                    if i == commit.authors.len() - 1 {
                        by.push_str(format!("and {}", author_display).as_str());
                    } else {
                        // 如果是倒数第二个，则不用添加逗号
                        if i == commit.authors.len() - 2 {
                            by.push_str(format!("{} ", author_display).as_str());
                        } else {
                            by.push_str(format!("{}, ", author_display).as_str());
                        }
                    }
                }
            }

            let mut hash = commit.hash.as_str().chars().take(7).collect::<String>();
            if !baseurl.is_empty() {
                hash = format!(" ([{}]({}/{}))", hash, baseurl, commit.hash);
            }
            // 如果 commit describuion 包含 (#xxx)，则将 hash 替换成空字符串
            let re = Regex::new(r"#\d+").unwrap();
            if re.is_match(commit.description.as_str()) {
                hash = "".to_string();
            }
            if commit.scope.is_empty() {
                changelog.push_str(format!("- {}{} - {}\n", commit.description, hash, by).as_str());
            } else {
                changelog.push_str(
                    format!(
                        "- **{}** {}{} - {}\n",
                        commit.scope, commit.description, hash, by
                    )
                    .as_str(),
                );
            }
        }
    }
    changelog.push_str("\n### :busts_in_silhouette: Contributors\n\n");
    for (_, contributor) in &contributors {
        if contributor.username.is_empty() {
            changelog.push_str(format!("- {} <{}>\n", contributor.name, contributor.mail).as_str());
        } else {
            changelog.push_str(
                format!(
                    "- {} (@{})\n",
                    contributor.name,
                    contributor.username.as_str()
                )
                .as_str(),
            );
        }
    }
    changelog
}

fn get_host_scope_repo(repo: &Repository, remote: &str) -> Option<(String, String, String)> {
    let remote_url = get_remote_url(repo, remote);
    if let Some(remote_url) = remote_url {
        let (host, scope, repo) = parse_git_url(&remote_url).unwrap();
        return Some((host.to_string(), scope.to_string(), repo.to_string()));
    }
    None
}

fn get_remote_url(repo: &Repository, remote: &str) -> Option<String> {
    let origin = repo.find_remote(remote);
    if let Ok(origin) = origin {
        let baseurl_str = origin.url().unwrap();
        let baseurl_string = &baseurl_str.to_string();
        return Some(baseurl_string.to_string());
    }
    None
}

fn organize_commit(
    revwalk: git2::Revwalk<'_>,
    repo: &Repository,
) -> (bool, HashMap<String, Author>, HashMap<String, Vec<Commit>>) {
    let mut has_breaking = false;
    // contributors is set of authors
    let mut contributors = HashMap::<String, Author>::new();
    let mut commit_map = HashMap::<String, Vec<Commit>>::new();
    for id in revwalk {
        let id = id.unwrap();
        let git_commit = repo.find_commit(id).unwrap();
        let author = git_commit.author();
        let commit = get_commit(&git_commit);
        let mail = author.email().unwrap();
        if contributors.contains_key(mail) {
            continue;
        }
        let name = fetch_github_username(mail);
        if let Ok(name) = name {
            let author = Author {
                name: author.name().unwrap().to_string(),
                mail: mail.to_string(),
                username: name,
            };
            contributors.insert(mail.to_string(), author);
        } else {
            let author = Author {
                name: author.name().unwrap().to_string(),
                mail: mail.to_string(),
                username: "".to_string(),
            };
            contributors.insert(mail.to_string(), author);
        }
        let commit = match commit {
            Some(commit) => commit,
            None => continue,
        };
        let commits = commit_map.entry(commit.type_.clone()).or_insert(Vec::new());
        if commit.is_breaking {
            has_breaking = true;
        }
        commits.push(commit);
    }
    (has_breaking, contributors, commit_map)
}

fn get_range<'a>(
    repo: &'a Repository,
    from: Option<String>,
    to: String,
    c2t: &'a HashMap<String, String>,
) -> Result<Vec<git2::Commit<'a>>, Box<dyn std::error::Error>> {
    let from_commit = get_from_commit(repo, from);
    let to_commit = get_from_commit(repo, Some(to.clone()));
    println!("from: {:?}", from_commit);
    println!("to: {:?}", to_commit);
    if from_commit.id() == to_commit.id() {
        return Err("No commits between from and to.".into());
    }

    let mut walker = repo.revwalk().unwrap();
    walker.push_range(format!("{}..{}", from_commit.id(), to_commit.id()).as_str())?;

    let mut commits = Vec::new();
    if !c2t.contains_key(from_commit.id().to_string().as_str()) {
        commits.push(from_commit);
    }
    for id in walker {
        let id = id.unwrap().to_string();
        if c2t.contains_key(id.as_str()) {
            let commit = repo.find_commit(id.parse().unwrap()).unwrap();
            commits.push(commit);
        }
    }
    let to_tag = c2t.get(to_commit.id().to_string().as_str());
    if None == to_tag {
        commits.push(to_commit);
    }
    Ok(commits)
}

fn get_from_commit(repo: &Repository, from: Option<String>) -> git2::Commit<'_> {
    let mut revwalk = repo.revwalk().unwrap();
    revwalk.push_head().unwrap();

    let from_commit;
    // 如果没有 from 参数，则获取最新的 tag。
    if from.is_none() {
        let mut latest_tag: Option<String> = None;
        let mut latest_commit = repo.head().unwrap().peel_to_commit().unwrap();
        for commit in revwalk {
            let commit = commit.unwrap();
            let commit = repo.find_commit(commit).unwrap();
            let tag = from_commit_get_tag(repo, &commit);
            latest_commit = commit;
            if tag.is_none() {
                continue;
            }
            if let Some(tag) = tag {
                latest_tag = Some(tag);
                break;
            }
        }
        if latest_tag.is_none() {
            from_commit = latest_commit;
        } else {
            // 获取最新 tag 对应的 commit。
            let tag = latest_tag.unwrap();
            let reference = repo.find_reference(&format!("refs/tags/{}", tag)).unwrap();
            from_commit = reference.peel_to_commit().unwrap();
        }
    } else {
        // 如果有 from 参数，则获取 from 对应的 commit。
        // 输入有可能是 tag 或是 commit 的 hash。
        let from = from.unwrap();
        let tags = repo.tag_names(Some(from.as_str())).unwrap();
        if tags.len() > 0 {
            let tag = tags.get(0).unwrap();
            let reference = repo.find_reference(&format!("refs/tags/{}", tag)).unwrap();
            from_commit = reference.peel_to_commit().unwrap();
        } else {
            from_commit = repo
                .revparse_single(from.as_str())
                .unwrap()
                .as_commit()
                .unwrap()
                .clone();
        }
    }
    from_commit
}

fn get_commit(commit: &git2::Commit) -> Option<Commit> {
    let message = commit.message().unwrap().lines().next().unwrap();
    let hash = commit.id().to_string();
    let author = commit.author();
    let author = Author {
        name: author.name().unwrap().to_string(),
        mail: author.email().unwrap().to_string(),
        username: "".to_string(),
    };
    let mut authors = vec![author];
    let body = commit.body();
    if !body.is_none() {
        let body = body.unwrap();
        parse_author_from_body(body, &mut authors);
    }
    let (_, scope, description, type_, is_breaking) = match parse_first_line(message) {
        Ok(value) => value,
        Err(value) => return value,
    };
    Some(Commit::new(
        hash,
        type_,
        scope,
        description,
        is_breaking,
        authors,
    ))
}

fn parse_first_line(
    message: &str,
) -> Result<(String, String, String, String, bool), Option<Commit>> {
    let first_line_regex = regex::Regex::new(r#"(?P<emoji>:.+:|(\u{1F300}-\u{1F3FF})|(\u{1F400}-\u{1F64F})|[\u{2600}-\u{2B55}])?( *)?(?P<type>[a-z]+)(\((?P<scope>.+)\))?(?P<breaking>!)?: (?P<description>.+)"#).unwrap();
    let captures = first_line_regex.captures(message);
    if captures.is_none() {
        return Err(None);
    }
    let captures = captures.unwrap();
    let scope = captures
        .name("scope")
        .map_or("", |m| m.as_str())
        .to_string();
    let description = captures
        .name("description")
        .map_or("", |m| m.as_str())
        .to_string();
    let type_ = captures.name("type").map_or("", |m| m.as_str()).to_string();
    let breaking = captures
        .name("breaking")
        .map_or("", |m| m.as_str())
        .to_string();
    let emoji = captures
        .name("emoji")
        .map_or("", |m| m.as_str())
        .to_string();
    let is_breaking = breaking == "!";
    Ok((emoji, scope, description, type_, is_breaking))
}

fn parse_author_from_body(body: &str, authors: &mut Vec<Author>) {
    for line in body.lines() {
        let author = match parse_author_from_line(line) {
            Some(value) => value,
            None => continue,
        };
        authors.push(author);
    }
}

fn parse_author_from_line(line: &str) -> Option<Author> {
    let co_authored_by_regex =
        regex::Regex::new(r#"Co-authored-by: (?P<name>.+) <(?P<mail>.+)>"#).unwrap();
    let captures = co_authored_by_regex.captures(line);
    if captures.is_none() {
        return None;
    }
    let captures = captures.unwrap();
    let name = captures.name("name").unwrap().as_str();
    let mail = captures.name("mail").unwrap().as_str();
    let author = Author {
        name: name.to_string(),
        mail: mail.to_string(),
        username: "".to_string(),
    };
    Some(author)
}

fn fetch_github_username(email: &str) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::blocking::Client::new();
    let url = format!("https://ungh.cc/users/find/{}", email);
    let response = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, "reqwest")
        .send()?;

    if !response.status().is_success() {
        return Err("Failed to fetch GitHub users".into());
    }

    let body = response.text()?;
    let data: Value = serde_json::from_str(&body)?;
    let user = data.get("user").unwrap_or(&Value::Null);
    let username = user
        .get("username")
        .unwrap_or(&Value::Null)
        .as_str()
        .unwrap();
    Ok(username.to_string())
}

// 单元测试模块
#[cfg(test)]
mod gitt_tests {
    use super::*;
    #[test]
    fn test_empty() {
        if let Err(err) = tgit(Options {
            from: None,
            to: "HEAD".to_string(),
            path: std::path::PathBuf::from("./repo/empty"),
            prefix: "".to_string(),
            remote: "origin".to_string(),
        }) {
            assert_eq!(err.to_string(), "The repository is empty.");
        }
    }

    #[test]
    fn test_has_untracked() {
        if let Err(err) = tgit(Options {
            from: None,
            to: "HEAD".to_string(),
            path: std::path::PathBuf::from("./repo/has_untracked"),
            prefix: "".to_string(),
            remote: "origin".to_string(),
        }) {
            assert_eq!(err.to_string(), "The repository has untracked files.");
        }
    }

    #[test]
    fn test_no_tag() {
        if let Err(err) = tgit(Options {
            from: None,
            to: "HEAD".to_string(),
            path: std::path::PathBuf::from("./repo/no_tag"),
            prefix: "".to_string(),
            remote: "origin".to_string(),
        }) {
            assert_eq!(err.to_string(), "No commits between from and to.");
        }
    }

    #[test]
    fn test_with_tag() {
        if let Err(_err) = tgit(Options {
            from: None,
            to: "HEAD".to_string(),
            path: std::path::PathBuf::from("./repo/with_tag"),
            prefix: "v".to_string(),
            remote: "origin".to_string(),
        }) {
        } else {
            assert!(true);
        }
    }
}
