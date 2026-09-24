//! Linhas do SQLite do tenant como structs. Tempo em ms (i64), JSON como
//! `serde_json::Value`, booleano como `bool` (0/1 no banco).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Enum com string canônica: o que vai para o banco é `as_str()`, nunca literal solto.
macro_rules! text_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name { $(#[serde(rename = $s)] $variant),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }

            pub fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some($name::$variant),)+ _ => None }
            }
        }
    };
}

text_enum!(Role { Viewer => "viewer", Agent => "agent", Manager => "manager", Admin => "admin" });

impl Role {
    pub fn level(self) -> u8 {
        match self {
            Role::Viewer => 1,
            Role::Agent => 2,
            Role::Manager => 3,
            Role::Admin => 4,
        }
    }

    pub fn at_least(self, min: Role) -> bool {
        self.level() >= min.level()
    }
}

text_enum!(LeadStatus { Open => "open", Won => "won", Lost => "lost" });

text_enum!(ConversationStatus {
    Open => "open",
    AiHandling => "ai_handling",
    Human => "human",
    Closed => "closed",
});

text_enum!(Direction { Inbound => "inbound", Outbound => "outbound" });

text_enum!(MessageType {
    Text => "text",
    Image => "image",
    Video => "video",
    Audio => "audio",
    Document => "document",
    Sticker => "sticker",
    Location => "location",
    Contacts => "contacts",
    Reaction => "reaction",
    Interactive => "interactive",
    Button => "button",
    Template => "template",
    System => "system",
    Unsupported => "unsupported",
});

text_enum!(MessageStatus {
    Queued => "queued",
    Accepted => "accepted",
    Sent => "sent",
    Delivered => "delivered",
    Read => "read",
    Failed => "failed",
    Received => "received",
});

impl MessageStatus {
    fn rank(self) -> Option<u8> {
        match self {
            MessageStatus::Queued => Some(0),
            MessageStatus::Accepted => Some(1),
            MessageStatus::Sent => Some(2),
            MessageStatus::Delivered => Some(3),
            MessageStatus::Read => Some(4),
            MessageStatus::Failed | MessageStatus::Received => None,
        }
    }

    /// Mesma regra do trigger `trg_messages_status_monotonic`. O teste
    /// `status_rule_matches_trigger` prova que os dois concordam.
    pub fn can_transition_to(self, next: MessageStatus) -> bool {
        use MessageStatus::*;
        if self == next {
            return true;
        }
        match (self, next) {
            (Read | Failed | Received, _) => false,
            (Delivered, Failed) => false,
            (_, Failed) => true,
            (a, b) => match (a.rank(), b.rank()) {
                (Some(x), Some(y)) => y >= x,
                _ => false,
            },
        }
    }
}

text_enum!(SentVia {
    Contact => "contact",
    Crm => "crm",
    Ai => "ai",
    Automation => "automation",
    Api => "api",
    ExternalDevice => "external_device",
    System => "system",
});

text_enum!(LinkTargetKind {
    Contact => "contact",
    Conversation => "conversation",
    Message => "message",
    Lead => "lead",
    Order => "order",
    Appointment => "appointment",
    External => "external",
});

text_enum!(OutboxKind {
    SendMessage => "send_message",
    SyncTemplates => "sync_templates",
    DownloadMedia => "download_media",
});

text_enum!(
    /// Vocabulário ABERTO: sem CHECK no banco. Tipo novo entra aqui,
    /// e todo emissor usa `as_str()`.
    ActivityType {
        LeadCreated => "lead_created",
        StageChanged => "stage_changed",
        StatusChanged => "status_changed",
        OwnerChanged => "owner_changed",
        NoteAdded => "note_added",
        MessageReceived => "message_received",
        MessageSent => "message_sent",
        HandoffToHuman => "handoff_to_human",
        HandoffToAi => "handoff_to_ai",
        OptOut => "opt_out",
        FieldChanged => "field_changed",
    }
);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub id: String,
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub phone_e164: Option<String>,
    pub wa_id: Option<String>,
    pub is_blocked: bool,
    pub blocked_reason: Option<String>,
    pub blocked_at: Option<i64>,
    pub is_anonymized: bool,
    pub anonymized_at: Option<i64>,
    pub merged_into_id: Option<String>,
    pub merged_at: Option<i64>,
    pub force_human: bool,
    pub consent: Value,
    pub source: String,
    pub source_metadata: Value,
    pub last_activity_at: Option<i64>,
    pub created_by: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pipeline {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub is_default: bool,
    pub is_archived: bool,
    pub position: String,
    pub vocabulary: Value,
    pub settings: Value,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stage {
    pub id: String,
    pub pipeline_id: String,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub position: String,
    pub color: Option<String>,
    pub is_won: bool,
    pub is_lost: bool,
    pub is_archived: bool,
    pub requires_human: bool,
    pub expected_duration_hours: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lead {
    pub id: String,
    pub pipeline_id: String,
    pub stage_id: String,
    pub contact_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub status: LeadStatus,
    pub lost_reason: Option<String>,
    pub position_in_stage: String,
    pub value_cents: Option<i64>,
    pub currency: String,
    pub owner_user_id: Option<String>,
    pub assigned_at: Option<i64>,
    /// `YYYY-MM-DD`
    pub expected_close_date: Option<String>,
    pub closed_at: Option<i64>,
    pub last_activity_at: Option<i64>,
    pub source: String,
    pub source_metadata: Value,
    pub external_id: Option<String>,
    pub custom_fields: Value,
    pub created_by: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeadActivity {
    pub id: String,
    pub lead_id: String,
    pub contact_id: Option<String>,
    /// Texto, não `ActivityType`: linha antiga pode ter tipo que o enum atual não conhece.
    #[serde(rename = "type")]
    pub kind: String,
    pub source_module: String,
    pub source_id: Option<String>,
    pub payload: Value,
    pub performed_by: Option<String>,
    pub performed_at: i64,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeadLink {
    pub id: String,
    pub lead_id: String,
    pub target_kind: LinkTargetKind,
    pub target_id: String,
    pub link_kind: String,
    pub metadata: Value,
    pub created_by: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub contact_id: String,
    pub phone_number_id: String,
    pub status: ConversationStatus,
    pub status_changed_at: i64,
    pub assigned_to_user_id: Option<String>,
    pub assigned_at: Option<i64>,
    pub last_inbound_at: Option<i64>,
    pub last_outbound_at: Option<i64>,
    pub last_message_at: Option<i64>,
    pub last_message_preview: Option<String>,
    pub unread_count: i64,
    pub bot_silenced_until: Option<i64>,
    pub last_handoff_at: Option<i64>,
    pub last_handoff_reason: Option<String>,
    pub snooze_until: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Conversation {
    pub fn service_window_open(&self, now_ms: i64) -> bool {
        crate::service_window_open(self.last_inbound_at, now_ms)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub contact_id: String,
    pub external_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub direction: Direction,
    #[serde(rename = "type")]
    pub kind: MessageType,
    pub status: MessageStatus,
    pub body: Option<String>,
    pub template_name: Option<String>,
    pub template_language: Option<String>,
    pub template_params: Option<Value>,
    pub media_r2_key: Option<String>,
    pub media_meta_id: Option<String>,
    pub media_mime: Option<String>,
    pub media_size_bytes: Option<i64>,
    pub reply_to_external_id: Option<String>,
    pub sent_via: SentVia,
    pub sent_by_user_id: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub sent_at: i64,
    pub accepted_at: Option<i64>,
    pub delivered_at: Option<i64>,
    pub read_at: Option<i64>,
    pub failed_at: Option<i64>,
    pub metadata: Value,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxItem {
    pub id: String,
    pub kind: OutboxKind,
    pub ref_id: String,
    pub payload: Value,
    pub attempts: i64,
    pub next_attempt_at: i64,
    pub last_error: Option<String>,
    pub created_at: i64,
}
