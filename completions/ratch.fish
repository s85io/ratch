# fish completion for ratch

function __ratch_sessions
    ratch list --names 2>/dev/null
end

function __ratch_all_sessions
    ratch list -a --names 2>/dev/null
end

set -l commands attach new start run push kill wait log tail clear list ls rm rename env current

complete -c ratch -f
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a "(__ratch_sessions)" -d session
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a attach -d "Attach to a running session"
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a new -d "Create a session and attach"
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a start -d "Create a session, detached"
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a run -d "Create a session, master in the foreground"
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a push -d "Send stdin to a session"
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a kill -d "Stop a session"
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a wait -d "Wait for a session to end"
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a log -d "Print a session's output"
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a tail -d "Print the end of a session's output"
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a clear -d "Forget a session's history"
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a list -d "List sessions"
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a rm -d "Remove what a finished session left"
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a rename -d "Rename a running session"
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a env -d "Refresh a shell's environment"
complete -c ratch -n "not __fish_seen_subcommand_from $commands" -a current -d "Print the current session"

complete -c ratch -n "__fish_seen_subcommand_from attach push kill wait clear rename env" -a "(__ratch_sessions)"
complete -c ratch -n "__fish_seen_subcommand_from log tail rm" -a "(__ratch_all_sessions)"

complete -c ratch -s e -x -d "Detach character"
complete -c ratch -s E -d "No detach character"
complete -c ratch -s n -x -d "History to show on attach"
complete -c ratch -l ro -d "Attach read-only"
complete -c ratch -s R -x -a "none clear" -d "Before attaching"
complete -c ratch -s r -x -a "none ctrl_l winch" -d "Redraw method"
complete -c ratch -s H -d "Keep the session open after the command ends"
complete -c ratch -s C -x -d "Log size limit"
complete -c ratch -s z -d "Do not interpret the suspend key"
complete -c ratch -s q -d "Suppress messages"
complete -c ratch -s t -d "Send the terminal no escape sequences"
complete -c ratch -n "__fish_seen_subcommand_from kill log tail" -s f -d "Force / follow"
complete -c ratch -n "__fish_seen_subcommand_from list ls rm" -s a -d "All"
complete -c ratch -n "__fish_seen_subcommand_from list ls" -l names -d "Names only"
complete -c ratch -n "__fish_seen_subcommand_from env" -l fish -d "fish syntax"
complete -c ratch -n "__fish_seen_subcommand_from env" -l sh -d "POSIX shell syntax"
