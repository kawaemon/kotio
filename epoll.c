#include <stdio.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

#include <sys/epoll.h>
#include <sys/timerfd.h>
#include <unistd.h>

#define fail(msg) do { fprintf(stderr, msg); exit(1); } while(false);
#define printfln(format, ...) printf(format "\n", __VA_ARGS__);

int main(void) {
    int timer1 = timerfd_create(CLOCK_BOOTTIME, 0);
    if (timer1 == -1) {
        fail("timer");
    }

    int timer2 = timerfd_create(CLOCK_BOOTTIME, 0);
    if (timer2 == -1) {
        fail("timer");
    }

    printfln("timer1 fd: %d", timer1);
    printfln("timer2 fd: %d", timer2);

    struct itimerspec timer_spec = {
        .it_interval = {
            .tv_sec = 3,
            .tv_nsec = 0,
        },
        .it_value = {
            .tv_sec = 3,
            .tv_nsec = 0,
        }
    };
    timerfd_settime(timer1, 0, &timer_spec, NULL);
    timer_spec = (struct itimerspec) {
        .it_interval = {
            .tv_sec = 5,
            .tv_nsec = 0,
        },
        .it_value = {
            .tv_sec = 5,
            .tv_nsec = 0,
        }
    };
    timerfd_settime(timer2, 0, &timer_spec, NULL);

    int epoll = epoll_create(1);
    if (epoll == -1) {
        fail("epoll");
    }

    struct epoll_event event;
    event.events = EPOLLIN;
    event.data.fd = timer1;

    if (epoll_ctl(epoll, EPOLL_CTL_ADD, timer1, &event) == -1) {
        fail("epoll ctl");
    }

    event.events = EPOLLIN;
    event.data.fd = timer2;

    if (epoll_ctl(epoll, EPOLL_CTL_ADD, timer2, &event) == -1) {
        fail("epoll ctl");
    }

    while(true) {
        #define EPOLL_MAX_EVENTS_ONCE 4

        struct epoll_event events[EPOLL_MAX_EVENTS_ONCE];

        int num_events_received = epoll_wait(epoll, events, EPOLL_MAX_EVENTS_ONCE, -1);
        if (num_events_received == -1) {
            fail("epoll wait");
        }

        printfln("received %d events", num_events_received);

        for (int i = 0; i < num_events_received; i++) {
            struct epoll_event event = events[i];

            uint64_t timer_read_buf;
            read(event.data.fd, &timer_read_buf, sizeof(timer_read_buf));
            printfln("read from %d: %lu", event.data.fd, timer_read_buf);
        }
    }

    close(epoll);
    close(timer1);
    close(timer2);
}
