use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Component, Path};

pub const DEFAULT_SKILL_TIMEOUT_SECS: u64 = 1800;

#[derive(Debug, Clone, Deserialize)]
pub struct SkillExecutionConfig {
    #[serde(default = "default_skill_timeout_secs")]
    pub default_timeout_secs: u64,
    #[serde(default)]
    pub skills: HashMap<String, SkillTimeoutOverride>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillTimeoutOverride {
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkillExecution {
    pub skill_name: String,
    pub timeout_secs: u64,
    pub source: &'static str,
}

pub const FOREGROUND_REQUIRED_ERROR: &str =
    "Skill 必须使用单次前台 exec 执行：禁止 setsid、nohup、后台 & 以及 sleep/ps/tail 轮询";
pub const TIMER_EXEC_POLICY_ERROR: &str = "code=rejected_execution_policy Timer exec policy rejected: 禁止后台执行、轮询以及通过临时 Python/Shell wrapper 间接运行 Skill";

fn default_skill_timeout_secs() -> u64 {
    DEFAULT_SKILL_TIMEOUT_SECS
}

impl Default for SkillExecutionConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: DEFAULT_SKILL_TIMEOUT_SECS,
            skills: HashMap::new(),
        }
    }
}

impl SkillExecutionConfig {
    pub fn timeout_for(&self, skill_name: &str) -> u64 {
        self.skills
            .get(skill_name)
            .map(|item| item.timeout_secs)
            .filter(|value| *value > 0)
            .unwrap_or({
                if self.default_timeout_secs > 0 {
                    self.default_timeout_secs
                } else {
                    DEFAULT_SKILL_TIMEOUT_SECS
                }
            })
    }

    pub fn source_for(&self, skill_name: &str) -> &'static str {
        if self
            .skills
            .get(skill_name)
            .is_some_and(|item| item.timeout_secs > 0)
        {
            "override"
        } else {
            "default"
        }
    }

    pub fn resolve(
        &self,
        command: &str,
        requested_timeout: Option<u64>,
    ) -> Option<ResolvedSkillExecution> {
        self.resolve_in_workspace(command, requested_timeout, None)
    }

    pub fn resolve_in_workspace(
        &self,
        command: &str,
        requested_timeout: Option<u64>,
        workspace: Option<&str>,
    ) -> Option<ResolvedSkillExecution> {
        let skill_name = identify_skill_in_workspace(command, workspace)?;
        let configured_timeout = self.timeout_for(&skill_name);
        let timeout_secs = requested_timeout
            .filter(|value| *value > 0)
            .map_or(configured_timeout, |value| value.min(configured_timeout));
        Some(ResolvedSkillExecution {
            timeout_secs,
            source: self.source_for(&skill_name),
            skill_name,
        })
    }
}

pub fn validate_foreground_skill_command(command: &str) -> Result<(), &'static str> {
    validate_foreground_skill_command_in_workspace(command, None)
}

pub fn validate_foreground_skill_command_in_workspace(
    command: &str,
    workspace: Option<&str>,
) -> Result<(), &'static str> {
    if identify_skill_in_workspace(command, workspace).is_none() {
        return Ok(());
    }
    if contains_forbidden_wrapper(command, 0)
        || contains_background_operator(command, 0)
        || contains_polling_command(command, 0)
    {
        return Err(FOREGROUND_REQUIRED_ERROR);
    }
    Ok(())
}

pub fn validate_timer_exec_command_in_workspace(command: &str, workspace: Option<&str>) -> Result<(), &'static str> {
    if contains_forbidden_wrapper(command, 0) || contains_background_operator(command, 0) || contains_polling_command(command, 0) || contains_timer_wrapper(command, workspace) { return Err(TIMER_EXEC_POLICY_ERROR); }
    Ok(())
}

pub fn identify_skill(command: &str) -> Option<String> {
    identify_skill_in_workspace(command, None)
}

pub fn identify_skill_in_workspace(command: &str, workspace: Option<&str>) -> Option<String> {
    split_shell_segments(command)
        .into_iter()
        .find_map(|segment| identify_skill_inner(segment, 0, workspace.map(Path::new)))
}

fn identify_skill_inner(command: &str, depth: usize, workspace: Option<&Path>) -> Option<String> {
    if depth > 1 {
        return None;
    }
    let tokens = shlex::split(command)?;
    let mut index = tokens
        .iter()
        .position(|token| !is_environment_assignment(token))?;

    if matches!(tokens[index].as_str(), "nohup" | "setsid") {
        index += 1;
    }
    let executable = tokens.get(index)?;
    let executable_name = Path::new(executable).file_name()?.to_str()?;
    if matches!(executable_name, "sh" | "bash" | "zsh")
        && tokens.get(index + 1).map(String::as_str) == Some("-c")
    {
        return identify_skill_inner(tokens.get(index + 2)?, depth + 1, workspace);
    }
    if !is_python_executable(executable_name) {
        return None;
    }

    let script = tokens.get(index + 1)?;
    if script.starts_with('-') {
        return None;
    }
    skill_name_from_path(script, workspace)
}

fn is_environment_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.chars().enumerate().all(|(index, ch)| {
            ch == '_' || ch.is_ascii_alphanumeric() && (index > 0 || !ch.is_ascii_digit())
        })
}

fn is_python_executable(name: &str) -> bool {
    name == "python"
        || name == "python3"
        || name.strip_prefix("python3.").is_some_and(|version| {
            !version.is_empty() && version.chars().all(|ch| ch.is_ascii_digit())
        })
}

fn skill_name_from_path(path: &str, workspace: Option<&Path>) -> Option<String> {
    let path = Path::new(path);
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }

    let skill = match parts.as_slice() {
        ["workspace", "skills", _category, skill, rest @ ..] if !rest.is_empty() => *skill,
        ["workspace", "_personal", "skills", skill, rest @ ..] if !rest.is_empty() => *skill,
        _ => {
            let relative = if path.is_absolute() {
                path.strip_prefix(workspace?).ok()?
            } else {
                workspace?;
                path
            };
            return skill_name_from_workspace_relative_path(relative);
        }
    };
    (!skill.is_empty()).then(|| skill.to_string())
}

fn skill_name_from_workspace_relative_path(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let skill = match parts.as_slice() {
        ["skills", _category, skill, rest @ ..] if !rest.is_empty() => *skill,
        ["_personal", "skills", skill, rest @ ..] if !rest.is_empty() => *skill,
        _ => return None,
    };
    (!skill.is_empty()).then(|| skill.to_string())
}

fn split_shell_segments(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut index = 0;
    let mut quote = None;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if byte == b'\\' && quote != Some(b'\'') {
            escaped = true;
            index += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            if quote == Some(byte) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(byte);
            }
            index += 1;
            continue;
        }
        if quote.is_none() && matches!(byte, b';' | b'\n' | b'|' | b'&') {
            if let Some(segment) = command
                .get(start..index)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                segments.push(segment);
            }
            index += 1;
            while index < bytes.len() && matches!(bytes[index], b'|' | b'&') {
                index += 1;
            }
            start = index;
            continue;
        }
        index += 1;
    }

    if let Some(segment) = command
        .get(start..)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        segments.push(segment);
    }
    segments
}

fn contains_background_operator(command: &str, depth: usize) -> bool {
    let bytes = command.as_bytes();
    let mut index = 0;
    let mut quote = None;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if byte == b'\\' && quote != Some(b'\'') {
            escaped = true;
            index += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            if quote == Some(byte) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(byte);
            }
            index += 1;
            continue;
        }
        if quote.is_none()
            && byte == b'&'
            && bytes.get(index.wrapping_sub(1)) != Some(&b'&')
            && bytes.get(index + 1) != Some(&b'&')
        {
            let redirected_fd = index > 0
                && bytes[index - 1] == b'>'
                && bytes.get(index + 1).is_some_and(u8::is_ascii_digit);
            if !redirected_fd {
                return true;
            }
        }
        index += 1;
    }
    if depth > 1 {
        return false;
    }
    split_shell_segments(command).into_iter().any(|segment| {
        let Some(tokens) = shlex::split(segment) else {
            return false;
        };
        let Some(index) = tokens
            .iter()
            .position(|token| !is_environment_assignment(token))
        else {
            return false;
        };
        let executable = tokens
            .get(index)
            .and_then(|token| Path::new(token).file_name())
            .and_then(|name| name.to_str());
        matches!(executable, Some("sh" | "bash" | "zsh"))
            && tokens.get(index + 1).map(String::as_str) == Some("-c")
            && tokens
                .get(index + 2)
                .is_some_and(|inner| contains_background_operator(inner, depth + 1))
    })
}

fn contains_forbidden_wrapper(command: &str, depth: usize) -> bool {
    command_segments_match(command, depth, |executable| {
        matches!(executable, "nohup" | "setsid")
    })
}

fn contains_polling_command(command: &str, depth: usize) -> bool {
    command_segments_match(command, depth, |executable| {
        matches!(executable, "sleep" | "ps" | "tail")
    })
}

fn contains_timer_wrapper(command: &str, workspace: Option<&str>) -> bool {
    split_shell_segments(command).into_iter().any(|segment| {
        let Some(tokens) = shlex::split(segment) else { return true; };
        let Some(index) = tokens.iter().position(|token| !is_environment_assignment(token)) else { return false; };
        let executable = tokens.get(index).and_then(|token| Path::new(token).file_name()).and_then(|name| name.to_str()).unwrap_or_default();
        if matches!(executable, "sh" | "bash" | "zsh") { return tokens.get(index + 1).is_some_and(|arg| arg == "-c" || is_temporary_wrapper_path(arg, workspace)); }
        is_python_executable(executable) && tokens.get(index + 1).is_some_and(|arg| is_temporary_wrapper_path(arg, workspace))
    })
}

fn is_temporary_wrapper_path(path: &str, workspace: Option<&str>) -> bool {
    let normalized = path.replace('\\', "/");
    if !normalized.ends_with(".py") && !normalized.ends_with(".sh") { return false; }
    normalized.starts_with("/workspace/work/") || normalized.starts_with("work/") || normalized.starts_with("./work/") || workspace.is_some_and(|root| normalized.starts_with(&format!("{}/work/", root.trim_end_matches('/'))))
}

fn command_segments_match(
    command: &str,
    depth: usize,
    predicate: impl Fn(&str) -> bool + Copy,
) -> bool {
    if depth > 1 {
        return false;
    }
    split_shell_segments(command).into_iter().any(|segment| {
        let Some(tokens) = shlex::split(segment) else {
            return false;
        };
        let Some(index) = tokens
            .iter()
            .position(|token| !is_environment_assignment(token))
        else {
            return false;
        };
        let Some(executable) = tokens
            .get(index)
            .and_then(|token| Path::new(token).file_name())
            .and_then(|name| name.to_str())
        else {
            return false;
        };
        if predicate(executable) {
            return true;
        }
        matches!(executable, "sh" | "bash" | "zsh")
            && tokens.get(index + 1).map(String::as_str) == Some("-c")
            && tokens
                .get(index + 2)
                .is_some_and(|inner| command_segments_match(inner, depth + 1, predicate))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_timeout_is_thirty_minutes() {
        let cfg = SkillExecutionConfig::default();
        assert_eq!(cfg.default_timeout_secs, 1800);
        assert_eq!(cfg.timeout_for("unknown-skill"), 1800);
    }

    #[test]
    fn skill_override_wins_over_default() {
        let cfg: SkillExecutionConfig = serde_yaml::from_str(
            "default_timeout_secs: 1800\nskills:\n  finance-payment:\n    timeout_secs: 2700\n",
        )
        .unwrap();
        assert_eq!(cfg.timeout_for("finance-payment"), 2700);
        assert_eq!(cfg.timeout_for("finance-revenue"), 1800);
    }

    #[test]
    fn zero_values_fall_back_without_discarding_other_overrides() {
        let cfg: SkillExecutionConfig = serde_yaml::from_str(
            "default_timeout_secs: 0\nskills:\n  bad:\n    timeout_secs: 0\n  good:\n    timeout_secs: 600\n",
        )
        .unwrap();
        assert_eq!(cfg.timeout_for("bad"), 1800);
        assert_eq!(cfg.timeout_for("good"), 600);
        assert_eq!(cfg.timeout_for("other"), 1800);
    }

    #[test]
    fn identifies_global_and_personal_skill_paths() {
        assert_eq!(
            identify_skill(
                "python3 /workspace/skills/finance/finance-payment/scripts/run_payment_report.py"
            )
            .as_deref(),
            Some("finance-payment")
        );
        assert_eq!(
            identify_skill(
                "python3 '/workspace/_personal/skills/my report/scripts/run.py' --date 20260825"
            )
            .as_deref(),
            Some("my report")
        );
    }

    #[test]
    fn identifies_host_and_relative_paths_under_configured_workspace() {
        assert_eq!(
            identify_skill_in_workspace(
                "python3 /srv/tyclaw/workspace/skills/finance/finance-payment/scripts/run.py",
                Some("/srv/tyclaw/workspace"),
            )
            .as_deref(),
            Some("finance-payment")
        );
        assert_eq!(
            identify_skill_in_workspace(
                "python3 skills/finance/finance-revenue/scripts/run.py",
                Some("/srv/tyclaw/workspace"),
            )
            .as_deref(),
            Some("finance-revenue")
        );
        assert_eq!(
            identify_skill_in_workspace(
                "python3 /srv/other/skills/finance/finance-payment/scripts/run.py",
                Some("/srv/tyclaw/workspace"),
            ),
            None
        );
    }

    #[test]
    fn identifies_environment_and_forbidden_wrappers() {
        for command in [
            "TY_CONFIG_PATH=/workspace/.config/ty.config.toml python3 /workspace/skills/finance/finance-revenue/scripts/run_ar_report.py",
            "nohup python3 /workspace/skills/finance/finance-payment/scripts/run_payment_report.py",
            "setsid python3 /workspace/skills/finance/finance-payment/scripts/run_payment_report.py",
            "bash -c 'python3 /workspace/skills/finance/finance-payment/scripts/run_payment_report.py'",
        ] {
            assert!(identify_skill(command).is_some(), "command={command}");
        }
    }

    #[test]
    fn identifies_skill_after_setup_commands_but_not_as_echo_data() {
        assert_eq!(
            identify_skill(
                "TODAY=20260825; mkdir -p work/tmp && python3 /workspace/skills/finance/finance-payment/scripts/run_payment_report.py --date $TODAY"
            )
            .as_deref(),
            Some("finance-payment")
        );
        assert_eq!(
            identify_skill(
                "echo python3 /workspace/skills/finance/finance-payment/scripts/run_payment_report.py"
            ),
            None
        );
    }

    #[test]
    fn rejects_non_skill_similar_and_escaping_paths() {
        for command in [
            "python3 /tmp/finance-payment/run_payment_report.py",
            "python3 /workspace/skills/finance/finance-payment/../../evil.py",
            "echo /workspace/skills/finance/finance-payment/scripts/run.py",
            "python3 -c 'print(1)'",
            "python3 -m finance-payment",
        ] {
            assert_eq!(identify_skill(command), None, "command={command}");
        }
    }

    #[test]
    fn resolves_skill_timeout_as_a_cap() {
        let cfg: SkillExecutionConfig = serde_yaml::from_str(
            "default_timeout_secs: 1800\nskills:\n  finance-payment:\n    timeout_secs: 2700\n",
        )
        .unwrap();
        let command =
            "python3 /workspace/skills/finance/finance-payment/scripts/run_payment_report.py";

        let capped = cfg.resolve(command, Some(9999)).unwrap();
        assert_eq!(capped.skill_name, "finance-payment");
        assert_eq!(capped.timeout_secs, 2700);
        assert_eq!(capped.source, "override");

        let shorter = cfg.resolve(command, Some(5)).unwrap();
        assert_eq!(shorter.timeout_secs, 5);

        let zero = cfg.resolve(command, Some(0)).unwrap();
        assert_eq!(zero.timeout_secs, 2700);
    }

    #[test]
    fn rejects_background_wrappers_and_polling_for_identified_skills() {
        for command in [
            "nohup python3 /workspace/skills/finance/a/scripts/run.py",
            "setsid python3 /workspace/skills/finance/a/scripts/run.py",
            "python3 /workspace/skills/finance/a/scripts/run.py &",
            "python3 /workspace/skills/finance/a/scripts/run.py 2>&1 &",
            "python3 /workspace/skills/finance/a/scripts/run.py; sleep 5; ps",
            "python3 /workspace/skills/finance/a/scripts/run.py | tail -f /tmp/a.log",
            "bash -c 'python3 /workspace/skills/finance/a/scripts/run.py &'",
        ] {
            assert!(
                validate_foreground_skill_command(command).is_err(),
                "command={command}"
            );
        }
    }

    #[test]
    fn allows_foreground_skill_with_output_redirection() {
        let command = "python3 /workspace/skills/finance/a/scripts/run.py > /tmp/a.log 2>&1";
        assert!(validate_foreground_skill_command(command).is_ok());
    }

    #[test]
    fn allows_foreground_skill_after_setup_with_and_operator() {
        let command = "mkdir -p work/tmp && python3 /workspace/skills/finance/a/scripts/run.py";
        assert!(validate_foreground_skill_command(command).is_ok());
    }

    #[test]
    fn allows_ampersand_inside_a_skill_argument() {
        let command = "python3 /workspace/skills/finance/a/scripts/run.py --title 'R&D report'";
        assert!(validate_foreground_skill_command(command).is_ok());
    }

    #[test]
    fn rejects_polling_inside_shell_wrapper_after_skill_setup() {
        let command = "python3 /workspace/skills/finance/a/scripts/run.py; bash -c 'sleep 5; ps'";
        assert!(validate_foreground_skill_command(command).is_err());
    }

    #[test]
    fn timer_exec_policy_rejects_detached_and_temporary_wrappers() {
        for command in ["setsid python3 /workspace/work/tmp/driver.py &", "sleep 20; ps -ef; tail work/tmp/run.log", "bash -c 'python3 /workspace/skills/finance/a/scripts/run.py'", "python3 /workspace/work/tmp/driver.py"] { assert!(validate_timer_exec_command_in_workspace(command, Some("/workspace")).is_err()); }
        assert!(validate_timer_exec_command_in_workspace("python3 /workspace/skills/finance/a/scripts/run.py", Some("/workspace")).is_ok());
    }
}
