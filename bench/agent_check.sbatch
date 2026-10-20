#!/bin/bash
#SBATCH --job-name=cdors-agentcheck
#SBATCH --partition=shared
#SBATCH --account=ab0995
#SBATCH --ntasks=1
#SBATCH --cpus-per-task=32
#SBATCH --hint=nomultithread
#SBATCH --mem=64G
#SBATCH --output=/scratch/a/a270088/cdors-bench/agentcheck-%j.out
# Agent check (bench/agent_check.sh) on a compute node instead of the login node; the sessions run one
# after another and need api.anthropic.com, which Levante compute nodes reach. TASKSET, TASKS, ARMS and
# CDORS_BIN come from the environment of sbatch (--export=ALL is the default), e.g.
#   TASKSET=fesom TASKS=F5 ARMS=A CDORS_BIN=<binary> sbatch --time=00:40:00 bench/agent_check.sbatch   # pilot
#   TASKSET=fesom CDORS_BIN=<binary> sbatch --time=02:30:00 bench/agent_check.sbatch                   # 10 sessions
#   TASKSET=trap CDORS_BIN=<binary> sbatch --time=01:30:00 bench/agent_check.sbatch                    # trap set, 10 sessions
#   TASKSET=long CDORS_BIN=<binary> sbatch --time=01:00:00 bench/agent_check.sbatch                    # H1, 2 sessions
cd /home/a/a270088/cdo || exit 1
export PATH=/home/a/a270088/.local/bin:$PATH
echo "start $(date -Is) on $(hostname): TASKSET=$TASKSET TASKS=${TASKS:-all} ARMS=${ARMS:-A B} CDORS_BIN=$CDORS_BIN"
RUN=1 bench/agent_check.sh
echo "end $(date -Is) rc=$?"
