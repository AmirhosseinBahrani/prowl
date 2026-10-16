ALTER TABLE reviews ADD COLUMN decision TEXT;
ALTER TABLE reviews ADD COLUMN decision_reasons TEXT;
ALTER TABLE reviews ADD COLUMN github_review_id INTEGER;
ALTER TABLE reviews ADD COLUMN published_url TEXT;
ALTER TABLE reviews ADD COLUMN published_at_ms INTEGER;
