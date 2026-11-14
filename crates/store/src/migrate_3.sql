-- Version 3. Each history row keeps what its item was right after the change it records, so the
-- copy to Trail says what happened then, not what the item is when the copy runs. Every write
-- fills them before it commits (engine::stamp_history); rows from before this version get the
-- item as it is now, which is what the copy sent for them until now.
ALTER TABLE history ADD COLUMN state TEXT;
ALTER TABLE history ADD COLUMN title TEXT;
ALTER TABLE history ADD COLUMN push_level TEXT;
ALTER TABLE history ADD COLUMN push_status TEXT;
ALTER TABLE history ADD COLUMN push_reason TEXT;
UPDATE history SET
    state = (SELECT state FROM items WHERE items.id = history.item),
    title = coalesce((SELECT title FROM items WHERE items.id = history.item), ''),
    push_level = (SELECT push_level FROM items WHERE items.id = history.item),
    push_status = (SELECT push_status FROM items WHERE items.id = history.item),
    push_reason = (SELECT push_reason FROM items WHERE items.id = history.item);
-- The rows a write still has to fill: few, so finding them is cheap.
CREATE INDEX history_unstamped ON history (id) WHERE title IS NULL;
