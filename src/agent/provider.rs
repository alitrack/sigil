//! 执行缝隙（Phase B，对齐 cc IProcessProvider / IFileSystemProvider 的
//! capability-seam 设计）：shell / fs 可替换 —— 本地 / 沙箱 / 远程 / 审计
//! 实现注入后，工具零改动。策略（权限、路径白名单）留在消费者侧。

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

/// 进程执行缝隙：运行命令并等待完成。`None` = 超时（子进程已被杀）。
#[async_trait]
pub trait ShellProvider: Send + Sync {
    async fn run(
        &self,
        cmd: tokio::process::Command,
        budget: std::time::Duration,
    ) -> anyhow::Result<Option<std::process::Output>>;
}

/// 记录型 shell（缝隙注入测试用）：不真正执行，返回固定输出。
/// tokio::process::Command 不暴露参数，调用次数即"走过缝隙"的证据。
#[cfg(test)]
pub struct RecordingShell {
    pub calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
#[async_trait]
impl ShellProvider for RecordingShell {
    async fn run(
        &self,
        _cmd: tokio::process::Command,
        _budget: std::time::Duration,
    ) -> anyhow::Result<Option<std::process::Output>> {
        use std::sync::atomic::Ordering;
        self.calls.fetch_add(1, Ordering::SeqCst);
        // 模拟 "bash -c 'echo hi'" → stdout "hi\n"
        Ok(Some(std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: b"hi\n".to_vec(),
            stderr: Vec::new(),
        }))
    }
}

/// 本地进程执行（默认实现，与旧 `tools::bash::run_with_timeout` 逐字节一致）。
pub struct LocalShellProvider;

#[async_trait]
impl ShellProvider for LocalShellProvider {
    async fn run(
        &self,
        cmd: tokio::process::Command,
        budget: std::time::Duration,
    ) -> anyhow::Result<Option<std::process::Output>> {
        let mut cmd = cmd;
        cmd.kill_on_drop(true);
        match tokio::time::timeout(budget, cmd.output()).await {
            Ok(Ok(out)) => Ok(Some(out)),
            Ok(Err(e)) => Err(anyhow::anyhow!("{e}")),
            Err(_) => Ok(None),
        }
    }
}

/// 文件系统执行缝隙。
#[async_trait]
pub trait FsProvider: Send + Sync {
    async fn read_to_string(&self, path: &Path) -> anyhow::Result<String>;

    async fn write(&self, path: &Path, contents: &str) -> anyhow::Result<()>;

    async fn create_dir_all(&self, path: &Path) -> anyhow::Result<()>;

    /// 供宿主/审计实现使用（当前内置工具尚未消费，接口完整性保留）。
    #[allow(dead_code)]
    async fn exists(&self, path: &Path) -> bool;
}

/// 本地文件系统（默认实现，tokio::fs 薄封装）。
pub struct LocalFsProvider;

#[async_trait]
impl FsProvider for LocalFsProvider {
    async fn read_to_string(&self, path: &Path) -> anyhow::Result<String> {
        Ok(tokio::fs::read_to_string(path).await?)
    }

    async fn write(&self, path: &Path, contents: &str) -> anyhow::Result<()> {
        Ok(tokio::fs::write(path, contents).await?)
    }

    async fn create_dir_all(&self, path: &Path) -> anyhow::Result<()> {
        Ok(tokio::fs::create_dir_all(path).await?)
    }

    async fn exists(&self, path: &Path) -> bool {
        tokio::fs::metadata(path).await.is_ok()
    }
}

/// 缝隙句柄类型（工具构造用）。
pub type ArcShell = Arc<dyn ShellProvider>;
pub type ArcFs = Arc<dyn FsProvider>;

/// 默认实现（无状态单例）。
pub fn default_shell() -> ArcShell {
    Arc::new(LocalShellProvider)
}

pub fn default_fs() -> ArcFs {
    Arc::new(LocalFsProvider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_shell_returns_output_for_fast_command() {
        let mut cmd = tokio::process::Command::new("echo");
        cmd.arg("hi");
        let out = LocalShellProvider
            .run(cmd, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        let out = out.expect("fast command must not time out");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    #[tokio::test]
    async fn local_shell_times_out_and_kills() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("30");
        let out = LocalShellProvider
            .run(cmd, std::time::Duration::from_millis(200))
            .await
            .unwrap();
        assert!(out.is_none(), "long command must time out");
    }

    #[tokio::test]
    async fn local_fs_read_write_exists_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sigil-provider-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");

        let fs = LocalFsProvider;
        assert!(!fs.exists(&path).await);
        fs.write(&path, "hello seam").await.unwrap();
        assert!(fs.exists(&path).await);
        assert_eq!(fs.read_to_string(&path).await.unwrap(), "hello seam");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
