# Animations

## Animations look cool btw.

Git is async on purpose. The prompt prints the path and runtime bits right
away, then Git comes back a moment later and the shell redraws the prompt with
`zle reset-prompt`. That is fast and correct, but the slight timing mismatch
still makes it somewhat unpleasant for the human eye.

I tried delaying it a little, and then tried revealing the Git text from left
to right. At a slow enough speed, it looks great. The effect itself is simple 
but quickly looks out of place.

The problem is that it runs after every command, `clear` etc., to make it feel
intentional I'd have to design a whole state machine keeping track of the
current git state and choosing fitting animations / parts to animate based
on the executed operation, the current state and the target state.

## More Specifically...

...the prompt would have to tell apart things like:

- opening a new shell or entering a project for the first time;
- changing to a different project or worktree;
- changing only the branch name;
- adding a small marker such as `⇡1`;
- removing a marker after the worktree becomes clean;
- leaving a repository completely.

Those should not all get the same animation. A first visit could reveal the
whole Git segment, while a small status change might only reveal the new
marker. A disappearing marker could be animated in reverse, but a branch or
project change might be better as an immediate replacement.

The Git value is already Zsh prompt text, not a plain string. It contains style
escapes like `%F{...}` and `%f`, escaped percent signs, symbols, Unicode, and
spacing. So “show one character at a time” cannot just split the string. It
would have to understand which parts are visible, preserve the Zsh escapes, and
deal with Unicode width and terminal wrapping.

Every frame also means another `zle reset-prompt`. A version that does not feel
broken would have to play nicely with:

- active line editing and cursor placement;
- wrapped and multi-line prompts;
- autosuggestions and syntax highlighting;
- `chpwd`, `preexec`, and generation cancellation;
- another Git result arriving before the first animation finishes;
- the existing 550 ms fetch deadline and worker cleanup.

It is doable, especially with AI nowadays. However, it is not clear to me that
it's worth the effort.
