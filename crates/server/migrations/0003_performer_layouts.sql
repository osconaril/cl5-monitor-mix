CREATE TABLE IF NOT EXISTS performer_layouts (
  user_name TEXT PRIMARY KEY,
  layout_json TEXT NOT NULL,
  FOREIGN KEY (user_name) REFERENCES users(name) ON DELETE CASCADE
);
