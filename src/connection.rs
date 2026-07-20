use crate::config::ServerConfig;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
pub struct ConnectionStats {
    pub total_connections: Arc<AtomicUsize>,
    pub active_connections: Arc<AtomicUsize>,
    pub total_messages: Arc<AtomicUsize>,
    pub rejected_connections: Arc<AtomicUsize>,
}

impl ConnectionStats {
    pub fn new() -> Self {
        Self {
            total_connections: Arc::new(AtomicUsize::new(0)),
            active_connections: Arc::new(AtomicUsize::new(0)),
            total_messages: Arc::new(AtomicUsize::new(0)),
            rejected_connections: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn increment_total(&self) {
        self.total_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_active(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn decrement_active(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn increment_messages(&self) {
        self.total_messages.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_rejected(&self) {
        self.rejected_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get_stats(&self) -> ConnectionStatsSnapshot {
        ConnectionStatsSnapshot {
            total_connections: self.total_connections.load(Ordering::Relaxed),
            active_connections: self.active_connections.load(Ordering::Relaxed),
            total_messages: self.total_messages.load(Ordering::Relaxed),
            rejected_connections: self.rejected_connections.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConnectionStatsSnapshot {
    pub total_connections: usize,
    pub active_connections: usize,
    pub total_messages: usize,
    pub rejected_connections: usize,
}

#[derive(Debug, Clone)]
pub struct ConnectionManager {
    config: ServerConfig,
    stats: ConnectionStats,
}

impl ConnectionManager {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            stats: ConnectionStats::new(),
        }
    }

    pub fn can_accept_connection(&self) -> bool {
        let active = self.stats.active_connections.load(Ordering::Relaxed);
        let max = self.config.server.max_connections;

        if active >= max {
            warn!(
                "Connection limit reached: {}/{}",
                active, max
            );
            return false;
        }

        true
    }

    pub async fn accept_connection(
        &self,
        stream: TcpStream,
        remote_addr: std::net::SocketAddr,
    ) -> Option<ManagedConnection> {
        self.stats.increment_total();

        if !self.can_accept_connection() {
            self.stats.increment_rejected();
            warn!("Rejected connection from {}", remote_addr);
            return None;
        }

        self.stats.increment_active();

        info!(
            "Accepted connection from {} (active: {})",
            remote_addr,
            self.stats.active_connections.load(Ordering::Relaxed)
        );

        Some(ManagedConnection {
            stream,
            remote_addr,
            start_time: Instant::now(),
            timeout: self.config.connection_timeout(),
            manager: self.clone(),
        })
    }

    pub fn connection_closed(&self, remote_addr: &std::net::SocketAddr) {
        self.stats.decrement_active();
        debug!(
            "Connection closed from {} (active: {})",
            remote_addr,
            self.stats.active_connections.load(Ordering::Relaxed)
        );
    }

    pub fn message_received(&self) {
        self.stats.increment_messages();
    }

    pub fn increment_rejected(&self) {
        self.stats.increment_rejected();
    }

    pub fn increment_total(&self) {
        self.stats.increment_total();
    }

    pub fn increment_active(&self) {
        self.stats.increment_active();
    }

    pub fn get_stats(&self) -> ConnectionStatsSnapshot {
        self.stats.get_stats()
    }

    pub fn print_stats(&self) {
        let stats = self.get_stats();
        info!("📊 Server Statistics:");
        info!("  Total Connections: {}", stats.total_connections);
        info!("  Active Connections: {}", stats.active_connections);
        info!("  Total Messages: {}", stats.total_messages);
        info!("  Rejected Connections: {}", stats.rejected_connections);
    }
}

#[derive(Debug)]
pub struct ManagedConnection {
    pub stream: TcpStream,
    pub remote_addr: std::net::SocketAddr,
    pub start_time: Instant,
    pub timeout: Duration,
    manager: ConnectionManager,
}

impl ManagedConnection {
    pub fn is_expired(&self) -> bool {
        self.start_time.elapsed() > self.timeout
    }

    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }
}

impl Drop for ManagedConnection {
    fn drop(&mut self) {
        self.manager.connection_closed(&self.remote_addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_stats() {
        let stats = ConnectionStats::new();

        stats.increment_total();
        stats.increment_active();
        stats.increment_messages();

        let snapshot = stats.get_stats();
        assert_eq!(snapshot.total_connections, 1);
        assert_eq!(snapshot.active_connections, 1);
        assert_eq!(snapshot.total_messages, 1);
        assert_eq!(snapshot.rejected_connections, 0);
    }

    #[test]
    fn test_connection_manager_limits() {
        let config = ServerConfig::default();
        let manager = ConnectionManager::new(config);

        // Should accept connection when under limit
        assert!(manager.can_accept_connection());
    }
}