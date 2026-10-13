# bash completion for ratch

_ratch() {
    local cur=${COMP_WORDS[COMP_CWORD]}
    local commands="attach new start run push kill wait log tail clear list ls rm rename env current"
    local command="" i
    for ((i = 1; i < COMP_CWORD; i++)); do
        case ${COMP_WORDS[i]} in
            -e | -r | -R | -n | -C) ((i++)) ;;
            -*) ;;
            *) command=${COMP_WORDS[i]}; break ;;
        esac
    done

    case ${COMP_WORDS[COMP_CWORD - 1]} in
        -r) COMPREPLY=($(compgen -W "none ctrl_l winch" -- "$cur")); return ;;
        -R) COMPREPLY=($(compgen -W "none clear" -- "$cur")); return ;;
        -e | -n | -C | -t) return ;;
    esac

    if [[ $cur == -* ]]; then
        COMPREPLY=($(compgen -W "-e -E -n --ro -R -r -H -C -z -q -t -f -a --names --sh --fish --help --version" -- "$cur"))
        return
    fi

    local running finished
    running=$(ratch list --names 2>/dev/null)
    case $command in
        "") COMPREPLY=($(compgen -W "$commands $running" -- "$cur")) ;;
        attach | push | kill | wait | clear | rename | env)
            COMPREPLY=($(compgen -W "$running" -- "$cur")) ;;
        log | tail | rm)
            finished=$(ratch list -a --names 2>/dev/null)
            COMPREPLY=($(compgen -W "$finished" -- "$cur")) ;;
        new | start | run)
            # After the session name comes a command to run.
            ((COMP_CWORD > i + 1)) && COMPREPLY=($(compgen -c -- "$cur")) ;;
    esac
}

complete -F _ratch ratch
