ALTER TABLE users ADD COLUMN last_sign_in TEXT;
CREATE TABLE IF NOT EXISTS user_mixes (
  user_name TEXT NOT NULL,
  mix_name TEXT NOT NULL,
  PRIMARY KEY (user_name, mix_name),
  FOREIGN KEY (user_name) REFERENCES users(name) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS admin_history (
  id INTEGER PRIMARY KEY,
  who TEXT NOT NULL,
  what TEXT NOT NULL,
  happened_at TEXT NOT NULL
);
