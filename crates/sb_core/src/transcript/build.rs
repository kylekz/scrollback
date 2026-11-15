//! [`Builder`]: how a provider makes a [`Conversation`].

use std::sync::Arc;

use super::{
    Block, BlockId, Content, Conversation, Ending, Tokens, Turn, Unreadable, Unrecognised,
};

/**
 * A conversation being made. Blocks are added in the order the agent recorded them, each
 * after the block it follows, and the builder keeps what the model promises of a conversation:
 *
 * - a block's parent is a block added before it;
 * - the shown branch is the blocks added as shown, in that order, and a turn starts at each
 *   prompt among them, so no turn is empty and only a conversation's first can lack a prompt;
 * - the models are listed once each, in the order they first answered.
 */
pub struct Builder {
    conversation: Conversation,
    /// The model of the block added last that had one, to share its name with the next.
    model: Option<Arc<str>>,
}

impl Builder {
    /// A conversation parsed from the item `item`.
    pub fn new(item: Arc<str>) -> Self {
        Self {
            conversation: Conversation {
                item,
                blocks: Vec::new(),
                turns: Vec::new(),
                unplaced: Vec::new(),
                models: Vec::new(),
                tokens: Tokens::default(),
                ending: Ending::Whole,
                unreadable: Vec::new(),
            },
            model: None,
        }
    }

    /// The key of the item the conversation is parsed from.
    pub fn item(&self) -> &Arc<str> {
        &self.conversation.item
    }

    /// Adds a block that follows `parent` and is on the shown branch.
    pub fn show(
        &mut self,
        parent: Option<BlockId>,
        at: Option<i64>,
        model: Option<&str>,
        content: Content,
    ) -> BlockId {
        let id = BlockId(self.conversation.blocks.len());
        match self.conversation.turns.last_mut() {
            Some(turn) if !matches!(content, Content::Prompt { .. }) => turn.blocks.push(id),
            _ => self.conversation.turns.push(Turn { blocks: vec![id] }),
        }
        self.hide(parent, at, model, content)
    }

    /// Adds a block that follows `parent` and is on a branch that isn't shown.
    pub fn hide(
        &mut self,
        parent: Option<BlockId>,
        at: Option<i64>,
        model: Option<&str>,
        content: Content,
    ) -> BlockId {
        let id = BlockId(self.conversation.blocks.len());
        assert!(
            parent.is_none_or(|parent| parent < id),
            "a block follows a block that was added before it"
        );
        let model = model.map(|name| match &self.model {
            Some(last) if **last == *name => last.clone(),
            _ => {
                if !self.conversation.models.iter().any(|model| model == name) {
                    self.conversation.models.push(name.to_string());
                }
                self.model.insert(Arc::from(name)).clone()
            }
        });
        self.conversation.blocks.push(Block {
            parent,
            at,
            model,
            content,
        });
        id
    }

    /// Keeps a record the provider doesn't know that has no place among the blocks.
    pub fn unplaced(&mut self, record: Unrecognised) {
        self.conversation.unplaced.push(record);
    }

    /// Counts what one of the conversation's requests used.
    pub fn used(&mut self, tokens: Tokens) {
        self.conversation.tokens.add(tokens);
    }

    /// Says that something that goes with the conversation could not be read.
    pub fn unreadable(&mut self, item: &str, reason: impl ToString) {
        self.conversation.unreadable.push(Unreadable {
            item: item.to_string(),
            reason: reason.to_string(),
        });
    }

    /// The conversation, its item read as far as `ending` says.
    pub fn finish(mut self, ending: Ending) -> Conversation {
        self.conversation.ending = ending;
        self.conversation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(text: &str) -> Content {
        Content::Prompt {
            text: text.to_string(),
            images: Vec::new(),
            from: None,
        }
    }

    fn reply(text: &str) -> Content {
        Content::Reply {
            text: text.to_string(),
        }
    }

    #[test]
    fn a_turn_starts_at_each_shown_prompt() {
        let mut builder = Builder::new("item".into());
        let note = builder.show(None, Some(5), None, reply("Before any prompt."));
        let first = builder.show(Some(note), None, None, prompt("One."));
        let answer = builder.show(Some(first), None, Some("model-1"), reply("First."));
        let rewound = builder.hide(Some(answer), None, None, prompt("Rewound."));
        builder.hide(Some(rewound), None, Some("model-2"), reply("Gone."));
        let second = builder.show(Some(answer), None, None, prompt("Two."));
        builder.show(Some(second), None, Some("model-1"), reply("Second."));
        builder.show(Some(second), None, Some("model-2"), reply("Again."));
        builder.used(Tokens {
            input: u64::MAX,
            output: 1,
            ..Tokens::default()
        });
        builder.used(Tokens {
            input: 1,
            output: 1,
            ..Tokens::default()
        });
        builder.unreadable("item.title", "gone");
        let conversation = builder.finish(Ending::MidRecord);

        let turns: Vec<Vec<usize>> = conversation
            .turns()
            .iter()
            .map(|turn| turn.blocks.iter().map(|id| id.0).collect())
            .collect();
        assert_eq!(turns, [vec![0], vec![1, 2], vec![5, 6, 7]]);
        assert_eq!(conversation.blocks().len(), 8);
        assert_eq!(conversation[second].parent, Some(answer));
        assert_eq!(conversation.models, ["model-1", "model-2"]);
        assert_eq!(conversation[answer].model.as_deref(), Some("model-1"));
        assert_eq!(conversation[first].model, None);
        assert_eq!(
            (conversation.tokens.input, conversation.tokens.output),
            (u64::MAX, 2)
        );
        assert_eq!(conversation.ending, Ending::MidRecord);
        assert_eq!(conversation.unreadable[0].item, "item.title");
        assert_eq!(conversation.span(), Some((5, 5)));
    }

    #[test]
    #[should_panic(expected = "a block follows a block that was added before it")]
    fn a_block_cannot_follow_one_that_is_not_there() {
        Builder::new("item".into()).show(Some(BlockId(0)), None, None, prompt("One."));
    }
}
