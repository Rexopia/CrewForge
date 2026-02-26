/// Build a basic system prompt for an agent with optional instructions.
pub fn build_system_prompt(
    agent_name: &str,
    instructions: Option<&str>,
) -> String {
    let mut prompt = format!("You are {agent_name}, an AI assistant.");
    if let Some(instr) = instructions {
        if !instr.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(instr);
        }
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_system_prompt_no_instructions() {
        let prompt = build_system_prompt("CrewForgeAgent", None);
        assert_eq!(prompt, "You are CrewForgeAgent, an AI assistant.");
    }

    #[test]
    fn build_system_prompt_with_instructions() {
        let prompt = build_system_prompt("CrewForgeAgent", Some("Be concise."));
        assert!(prompt.contains("CrewForgeAgent"));
        assert!(prompt.contains("Be concise."));
    }

    #[test]
    fn build_system_prompt_empty_instructions() {
        let prompt = build_system_prompt("CrewForgeAgent", Some(""));
        assert_eq!(prompt, "You are CrewForgeAgent, an AI assistant.");
    }
}
