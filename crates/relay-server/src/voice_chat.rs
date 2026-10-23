//! Durable voice text uses an ordinary separate text target, never call key state.
use super::*;
#[cfg(test)]
mod tests;
pub(super) fn ensure_rooms(db: &Connection) -> rusqlite::Result<()> {
    let mut query=db.prepare("SELECT id,name FROM channels WHERE kind='voice' AND NOT EXISTS(SELECT 1 FROM channels child WHERE child.voice_parent_id=channels.id)")?;
    let rooms = query
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    for (parent, name) in rooms {
        db.execute("INSERT INTO channels(id,name,kind,topic,visibility,voice_parent_id) VALUES(?1,?2,'text','Text chat in this voice channel','private',?3)",params![id(),format!("{name} chat"),parent])?;
    }
    Ok(())
}
