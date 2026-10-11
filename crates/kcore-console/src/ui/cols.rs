//! Width-aware column sets for Network and Storage tables.

use ratatui::layout::Constraint;

/// Network table columns. Order matches left-to-right display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetColumn {
    Iface,
    State,
    Mac,
    Ipv4,
    Ipv6,
    Mtu,
    Speed,
    Driver,
    Master,
    Def,
    Mgmt,
}

impl NetColumn {
    pub fn header(self) -> &'static str {
        match self {
            Self::Iface => "Iface",
            Self::State => "State",
            Self::Mac => "MAC",
            Self::Ipv4 => "IPv4",
            Self::Ipv6 => "IPv6",
            Self::Mtu => "MTU",
            Self::Speed => "Speed",
            Self::Driver => "Driver",
            Self::Master => "master",
            Self::Def => "def",
            Self::Mgmt => "mgmt",
        }
    }

    pub fn constraint(self, narrow_flags: bool) -> Constraint {
        match self {
            Self::Iface => Constraint::Min(8),
            Self::State => Constraint::Length(5),
            Self::Mac => Constraint::Length(17),
            // Worst-case IPv4 dotted-quad + /prefix = 18; never shrink.
            Self::Ipv4 => Constraint::Length(18),
            Self::Ipv6 => Constraint::Fill(1),
            Self::Mtu => Constraint::Length(5),
            Self::Speed => Constraint::Length(10),
            Self::Driver => Constraint::Length(8),
            Self::Master => Constraint::Length(8),
            Self::Def | Self::Mgmt => {
                if narrow_flags {
                    Constraint::Length(1)
                } else {
                    Constraint::Length(3)
                }
            }
        }
    }
}

/// Pick the Network column set from the drawable area width (inner table width).
pub fn net_columns_for_width(width: u16) -> &'static [NetColumn] {
    if width >= 160 {
        &[
            NetColumn::Iface,
            NetColumn::State,
            NetColumn::Mac,
            NetColumn::Ipv4,
            NetColumn::Ipv6,
            NetColumn::Mtu,
            NetColumn::Speed,
            NetColumn::Driver,
            NetColumn::Master,
            NetColumn::Def,
            NetColumn::Mgmt,
        ]
    } else if width >= 110 {
        // 128x48 framebuffer case: drop Driver/MTU; master shown as iface suffix.
        &[
            NetColumn::Iface,
            NetColumn::State,
            NetColumn::Mac,
            NetColumn::Ipv4,
            NetColumn::Ipv6,
            NetColumn::Speed,
            NetColumn::Def,
            NetColumn::Mgmt,
        ]
    } else {
        // 80x25: no MAC; flags as single characters; IPv6 is primary only.
        &[
            NetColumn::Iface,
            NetColumn::State,
            NetColumn::Ipv4,
            NetColumn::Ipv6,
            NetColumn::Def,
            NetColumn::Mgmt,
        ]
    }
}

/// Whether Iface cells should append ` -> master`.
pub fn net_iface_shows_master_suffix(cols: &[NetColumn]) -> bool {
    !cols.contains(&NetColumn::Master)
}

/// Storage table columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskColumn {
    Name,
    Path,
    Model,
    Serial,
    Size,
    Kind,
    Ro,
    Mounts,
    Health,
    Role,
}

impl DiskColumn {
    pub fn header(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Path => "Path",
            Self::Model => "Model",
            Self::Serial => "Serial",
            Self::Size => "Size",
            Self::Kind => "Kind",
            Self::Ro => "RO",
            Self::Mounts => "Mounts",
            Self::Health => "Health",
            Self::Role => "Role",
        }
    }

    pub fn constraint(self) -> Constraint {
        match self {
            Self::Name => Constraint::Length(8),
            Self::Path => Constraint::Min(8),
            Self::Model => Constraint::Min(10),
            Self::Serial => Constraint::Min(8),
            Self::Size => Constraint::Length(9),
            Self::Kind => Constraint::Length(5),
            Self::Ro => Constraint::Length(2),
            Self::Mounts => Constraint::Fill(1),
            Self::Health => Constraint::Length(7),
            Self::Role => Constraint::Length(8),
        }
    }
}

pub fn disk_columns_for_width(width: u16) -> &'static [DiskColumn] {
    if width >= 160 {
        &[
            DiskColumn::Name,
            DiskColumn::Path,
            DiskColumn::Model,
            DiskColumn::Serial,
            DiskColumn::Size,
            DiskColumn::Kind,
            DiskColumn::Ro,
            DiskColumn::Mounts,
            DiskColumn::Health,
            DiskColumn::Role,
        ]
    } else if width >= 110 {
        &[
            DiskColumn::Name,
            DiskColumn::Path,
            DiskColumn::Size,
            DiskColumn::Kind,
            DiskColumn::Health,
            DiskColumn::Role,
        ]
    } else {
        &[
            DiskColumn::Name,
            DiskColumn::Size,
            DiskColumn::Kind,
            DiskColumn::Role,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_column_set_by_width() {
        let wide: Vec<_> = net_columns_for_width(200).to_vec();
        assert!(wide.contains(&NetColumn::Driver));
        assert!(wide.contains(&NetColumn::Master));
        assert!(wide.contains(&NetColumn::Mac));

        let mid: Vec<_> = net_columns_for_width(128).to_vec();
        assert!(!mid.contains(&NetColumn::Driver));
        assert!(!mid.contains(&NetColumn::Mtu));
        assert!(!mid.contains(&NetColumn::Master));
        assert!(mid.contains(&NetColumn::Mac));
        assert!(mid.contains(&NetColumn::Ipv4));
        assert!(net_iface_shows_master_suffix(&mid));

        let narrow: Vec<_> = net_columns_for_width(80).to_vec();
        assert!(!narrow.contains(&NetColumn::Mac));
        assert!(!narrow.contains(&NetColumn::Speed));
        assert_eq!(
            narrow,
            vec![
                NetColumn::Iface,
                NetColumn::State,
                NetColumn::Ipv4,
                NetColumn::Ipv6,
                NetColumn::Def,
                NetColumn::Mgmt,
            ]
        );
    }

    #[test]
    fn disk_column_set_by_width() {
        assert_eq!(disk_columns_for_width(200).len(), 10);
        assert_eq!(disk_columns_for_width(128).len(), 6);
        assert_eq!(disk_columns_for_width(80).len(), 4);
        assert!(disk_columns_for_width(80).contains(&DiskColumn::Size));
    }

    #[test]
    fn ipv4_column_is_fixed_length_18() {
        assert_eq!(NetColumn::Ipv4.constraint(false), Constraint::Length(18));
    }
}
