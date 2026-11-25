//! Each turn's final message on disk, and `session_turn`.

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn a_stop_raises_turn_and_keeps_the_rest() {
        let before = Counts {
            turn: 4,
            prompts: 9,
            question: true,
        };
        assert_eq!(
            before.after_stop(),
            Counts {
                turn: 5,
                prompts: 9,
                question: true
            }
        );
    }

    #[test]
    fn a_prompt_adds_one_and_clears_the_question() {
        let before = Counts {
            turn: 4,
            prompts: 9,
            question: true,
        };
        assert_eq!(
            before.after_prompt(),
            Counts {
                turn: 4,
                prompts: 10,
                question: false
            }
        );
    }

    #[test]
    fn a_question_sets_the_flag_and_keeps_the_counts() {
        let before = Counts {
            turn: 4,
            prompts: 9,
            question: false,
        };
        assert_eq!(
            before.after_question(),
            Counts {
                turn: 4,
                prompts: 9,
                question: true
            }
        );
    }

    #[test]
    fn hide_each_replaces_every_secret_and_skips_empty_ones() {
        let text = "key K1 and K2 and K1 again";
        assert_eq!(
            hide_each(text, &["K1", "", "K2"]),
            "key *** and *** and *** again"
        );
        assert_eq!(hide_each(text, &[]), text);
    }

    #[test]
    fn a_text_file_is_replaced_whole_and_private_and_a_stale_new_file_is_no_matter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("turns").join("1.txt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path.with_extension("txt.new"), "stale").unwrap();

        write_text(&path, "first").unwrap();
        write_text(&path, "second").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!path.with_extension("txt.new").exists());
    }

    #[test]
    fn a_turn_and_the_question_have_their_own_files_in_the_session_folder() {
        let folder = std::path::Path::new("/data/sessions/s");
        assert_eq!(turn_file(folder, 3), folder.join("turns").join("3.txt"));
        assert_eq!(question_file(folder), folder.join("question.txt"));
    }
}
