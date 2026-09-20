-- Reference copy of the tables claude-job-runner works with (MySQL 8).
--
-- The source of truth is the granite-webhooks repository:
--   migrations/20260920153120_log-chats.sql                   (chats, chat_messages)
--   migrations/20260920170000_alter-chat-messages-add-status.sql (status column)
--
-- `chats` is reproduced here without its foreign keys to `users` and
-- `company` so the file can stand alone; the integration tests load it into
-- a throw-away database. The runner only ever reads and writes
-- `chat_messages`.

CREATE TABLE IF NOT EXISTS chats (
  id INT AUTO_INCREMENT PRIMARY KEY,
  user_id INT NOT NULL,
  company_id INT NOT NULL,
  title VARCHAR(200) NOT NULL DEFAULT 'New chat',
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  INDEX idx_chats_user_updated (user_id, updated_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- sender is 'user' or 'ai'. A row with sender = 'user' and is_agentic = 1 is
-- a job for the runner: status NULL = not picked up yet, then 'pending' ->
-- 'done' | 'failed'. The answer is a new sender = 'ai' row in the same chat.
CREATE TABLE IF NOT EXISTS chat_messages (
  id INT AUTO_INCREMENT PRIMARY KEY,
  chat_id INT NOT NULL,
  sender ENUM('user', 'ai') NOT NULL,
  content TEXT NOT NULL,
  is_agentic TINYINT(1) NULL DEFAULT NULL,
  payload JSON NULL,
  status ENUM('pending', 'done', 'failed') NULL DEFAULT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_chat_messages_chat_created (chat_id, created_at),
  INDEX idx_chat_messages_pickup (sender, is_agentic, status),
  CONSTRAINT fk_chat_messages_chat
    FOREIGN KEY (chat_id)
    REFERENCES chats(id)
    ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
