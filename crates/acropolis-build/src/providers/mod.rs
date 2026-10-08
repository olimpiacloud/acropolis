pub mod go;
pub mod images;
pub mod node;
pub mod php;
pub mod python;
pub mod ruby;
pub mod rust;
pub mod simple;

pub fn shell_start(start: &str) -> Vec<String> {
    let t = start.trim();
    let first = t.split_whitespace().next().unwrap_or("");
    let single = !t.is_empty()
        && !t.contains(['&', ';', '|', '\n', '`', '(', ')', '<', '>'])
        && !first.contains('=')
        && !matches!(
            first,
            "exec"
                | "cd"
                | "export"
                | "source"
                | "."
                | "set"
                | "ulimit"
                | "umask"
                | "trap"
                | "eval"
                | "if"
                | "for"
                | "while"
                | "case"
        );
    let cmd = if single { format!("exec {t}") } else { t.to_string() };
    vec!["/bin/sh".into(), "-c".into(), cmd]
}

#[cfg(test)]
mod tests {
    use super::shell_start;

    #[test]
    fn exec_only_single_commands() {
        assert_eq!(
            shell_start("gunicorn app:app --bind 0.0.0.0:$PORT")[2],
            "exec gunicorn app:app --bind 0.0.0.0:$PORT"
        );
        assert_eq!(
            shell_start("python manage.py migrate && gunicorn x")[2],
            "python manage.py migrate && gunicorn x"
        );
        assert_eq!(shell_start("FOO=1 python main.py")[2], "FOO=1 python main.py");
        assert_eq!(shell_start("cd src && python main.py")[2], "cd src && python main.py");
        assert_eq!(shell_start("exec python main.py")[2], "exec python main.py");
    }
}
