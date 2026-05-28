-- Seed IronClaw workspace with identity files from the Olson Life System
-- Run: psql -U postgres -h 127.0.0.1 -p 5432 -d ironclad -f scripts/seed_workspace.sql

-- Insert identity documents into memory_documents
INSERT INTO memory_documents (user_id, path, content, created_at, updated_at)
VALUES
  ('default', 'USER.md', pg_read_file('C:/Users/miksu/Documents/Olson/.ironclad/workspace_seed/USER.md'), NOW(), NOW()),
  ('default', 'SOUL.md', pg_read_file('C:/Users/miksu/Documents/Olson/.ironclad/workspace_seed/SOUL.md'), NOW(), NOW()),
  ('default', 'IDENTITY.md', pg_read_file('C:/Users/miksu/Documents/Olson/.ironclad/workspace_seed/IDENTITY.md'), NOW(), NOW()),
  ('default', 'AGENTS.md', pg_read_file('C:/Users/miksu/Documents/Olson/.ironclad/workspace_seed/AGENTS.md'), NOW(), NOW()),
  ('default', 'HEARTBEAT.md', pg_read_file('C:/Users/miksu/Documents/Olson/.ironclad/workspace_seed/HEARTBEAT.md'), NOW(), NOW()),
  ('default', 'MEMORY.md', pg_read_file('C:/Users/miksu/Documents/Olson/.ironclad/workspace_seed/MEMORY.md'), NOW(), NOW()),
  ('default', 'README.md', pg_read_file('C:/Users/miksu/Documents/Olson/.ironclad/workspace_seed/README.md'), NOW(), NOW())
ON CONFLICT (user_id, path) WHERE agent_id IS NULL
DO UPDATE SET content = EXCLUDED.content, updated_at = NOW();
