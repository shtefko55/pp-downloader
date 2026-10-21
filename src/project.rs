//! Проект = папка с клиповете на едно предаване + файл `<име>.ppdl` (JSON)
//! със списъка откъси, редакциите им и къде са експортирани.

use crate::Job;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const EXT: &str = "ppdl";

#[derive(Serialize, Deserialize, Default)]
pub struct Project {
    pub name: String,
    pub jobs: Vec<Job>,
    pub next_id: u64,
    #[serde(skip)]
    pub path: PathBuf,
}

impl Project {
    pub fn dir(&self) -> PathBuf {
        self.path.parent().map(Path::to_path_buf).unwrap_or_default()
    }

    /// Папката по подразбиране за готовите (експортирани) файлове.
    pub fn exports_dir(&self) -> PathBuf {
        self.dir().join("Готови")
    }

    /// Нов проект в собствена папка под `root`; ако вече съществува — отваря го.
    pub fn create(root: &Path, name: &str) -> Result<Project, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("Въведи име на проекта".into());
        }
        let folder = crate::worker::sanitize(name);
        let dir = root.join(&folder);
        let path = dir.join(format!("{folder}.{EXT}"));
        if path.is_file() {
            return Project::load(&path);
        }
        std::fs::create_dir_all(&dir).map_err(|e| format!("Папката на проекта не може да се създаде: {e}"))?;
        let project = Project { name: name.to_string(), jobs: Vec::new(), next_id: 0, path };
        project.save()?;
        Ok(project)
    }

    pub fn load(path: &Path) -> Result<Project, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("Проектът не може да се прочете: {e}"))?;
        let mut v: Value = serde_json::from_str(&text).map_err(|e| format!("Повреден файл на проект: {e}"))?;
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        map_paths(&mut v, |p| {
            if Path::new(p.as_str()).is_relative() {
                *p = dir.join(p.as_str()).display().to_string();
            }
        });
        let mut project: Project = serde_json::from_value(v).map_err(|e| format!("Повреден файл на проект: {e}"))?;
        project.path = path.to_path_buf();
        Ok(project)
    }

    /// JSON, в който пътищата вътре в папката на проекта са относителни —
    /// така папката може да се премести на друг диск или компютър.
    pub fn to_json(&self) -> String {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        let dir = self.dir();
        map_paths(&mut v, |p| {
            if let Ok(rel) = Path::new(p.as_str()).strip_prefix(&dir) {
                *p = rel.display().to_string();
            }
        });
        serde_json::to_string_pretty(&v).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        write_atomic(&self.path, &self.to_json())
    }
}

/// Записва през временен файл, за да не остане полузаписан проект при срив.
pub fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("Проектът не може да се запише: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Проектът не може да се запише: {e}"))
}

/// Пътищата във файла: свален клип (`done[0]`) и последен експорт (`edit.exported`).
fn map_paths(v: &mut Value, f: impl Fn(&mut String)) {
    let jobs = v.get_mut("jobs").and_then(Value::as_array_mut).into_iter().flatten();
    for job in jobs {
        for pointer in ["/done/0", "/edit/exported"] {
            if let Some(Value::String(s)) = job.pointer_mut(pointer) {
                f(s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::EditState;

    #[test]
    fn paths_are_relative_in_file() {
        let root = std::env::temp_dir().join("pp_project_test");
        let _ = std::fs::remove_dir_all(&root);
        let mut p = Project::create(&root, "Предаване 100").unwrap();
        let dir = p.dir();
        let mut job = crate::new_job(1);
        job.url = "https://example.com/v".into();
        job.done = Some((dir.join("клип [00-01-00_00-02-00].mp4"), 5.0, 65.0));
        job.edit = Some(EditState { in_t: 5.2, out_t: 64.0, masks: Vec::new(), exported: Some(dir.join("Готови").join("Финал.mp4")) });
        p.jobs.push(job);
        p.save().unwrap();

        let text = std::fs::read_to_string(&p.path).unwrap();
        assert!(!text.contains("pp_project_test"), "абсолютен път във файла:\n{text}");

        // Преместване на цялата папка → пътищата следват.
        let moved = root.join("преместен");
        std::fs::rename(&dir, &moved).unwrap();
        let q = Project::load(&moved.join("Предаване 100.ppdl")).unwrap();
        assert_eq!(q.name, "Предаване 100");
        assert_eq!(q.jobs[0].done.as_ref().unwrap().0, moved.join("клип [00-01-00_00-02-00].mp4"));
        let edit = q.jobs[0].edit.as_ref().unwrap();
        assert_eq!(edit.exported.as_ref().unwrap(), &moved.join("Готови").join("Финал.mp4"));
        assert_eq!(edit.in_t, 5.2);

        // Повторно „създаване“ със същото име отваря съществуващия.
        let again = Project::create(&root, "Предаване 100").unwrap();
        assert_eq!(again.path, root.join("Предаване 100").join("Предаване 100.ppdl"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
