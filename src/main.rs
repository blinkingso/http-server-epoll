use std::{
    collections::HashMap,
    io::{self, ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    os::fd::{AsRawFd, RawFd},
};

#[macro_use]
mod macros;

const HTTP_RESP: &[u8] = b"HTTP/1.0 200 OK
content-type: text/html
content-length: 5

Hello";

fn main() -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:8080")?;
    // 设置为非阻塞模式: 当调用accept方法时, 如果数据未就绪那么会返回一个io::ErrorKind::WouldBlock错误
    // 不会阻塞当前的线程.
    listener.set_nonblocking(true)?;
    // socket fd
    let socket_fd = listener.as_raw_fd();
    println!("server socket fd: {}", socket_fd);
    // epoll fd
    let epoll_fd = epoll_create()?;
    let mut key = 100;
    add_interest(epoll_fd, socket_fd, listener_read_event(key))?;

    let mut events: Vec<libc::epoll_event> = Vec::with_capacity(1024);
    // 保存客户端列表
    let mut request_contexts: HashMap<u64, RequestContext> = HashMap::new();
    loop {
        events.clear();
        let res = match syscall!(epoll_wait(
            epoll_fd,
            events.as_mut_ptr() as *mut libc::epoll_event,
            // 最大事件数量
            1024,
            // -1 表示一直等待, 0表示立即返回, 1000表示等待1s
            -1 as libc::c_int
        )) {
            Ok(v) => v,
            Err(e) => panic!("error during epoll wait: {:?}", e),
        };

        // safe as long as kernel does nothing wrong, copied from mio;
        unsafe { events.set_len(res as usize) }
        println!("events in: {}", res);

        for event in events.iter() {
            match event.u64 {
                100 => {
                    match listener.accept() {
                        Ok((mut stream, address)) => {
                            stream.set_nonblocking(true)?;
                            println!("new connection: {}", address);
                            key += 1;

                            add_interest(epoll_fd, stream.as_raw_fd(), listener_read_event(key))?;
                            request_contexts.insert(key, RequestContext::new(stream));
                        }
                        Err(e) => eprintln!("We couldn't accpet a connection for: {}", e),
                    }
                    modify_interest(epoll_fd, socket_fd, listener_read_event(100))?;
                }
                // client socket
                key => {
                    let mut to_delete = None;
                    if let Some(context) = request_contexts.get_mut(&key) {
                        let event_code = event.events;
                        match event_code {
                            // read
                            v if v as i32 & libc::EPOLLIN == libc::EPOLLIN => {
                                context.read_cb(key, epoll_fd)?;
                            }
                            v if v as i32 & libc::EPOLLOUT == libc::EPOLLOUT => {
                                context.write_cb(key, epoll_fd)?;
                                to_delete = Some(key);
                            }
                            v => println!("Unexpected events: {}", v),
                        }
                    }
                    // http已经write back
                    if let Some(key) = to_delete {
                        request_contexts.remove(&key);
                    }
                }
            }
        }
    }
}

/**
 * 调用系统api
 * ```
 *  int fd = epoll_create1(size); // 返回epoll事件通知工具的fd.
 * ```
 * 当调用epoll_wait时, 可以通过该epoll_fd去添加, 删除或者修改感兴趣的socket_fd的事件. 该方法会阻塞
 * 直到有事件发生, 通知我们之后我们就可以读取socket_fd的事件: 比如新的连接, 读数据, 写数据.
 */
fn epoll_create() -> io::Result<RawFd> {
    let epoll_fd = syscall!(epoll_create1(0))?;
    println!("epoll_fd: {}", epoll_fd);
    if let Ok(flags) = syscall!(fcntl(epoll_fd, libc::F_GETFD)) {
        let _ = syscall!(fcntl(epoll_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC));
    }
    Ok(epoll_fd)
}

// 通过系统调用epoll_ctl添加事件监听到epoll队列
fn add_interest(epoll_fd: RawFd, socket_fd: RawFd, mut event: libc::epoll_event) -> io::Result<()> {
    syscall!(epoll_ctl(
        epoll_fd,
        libc::EPOLL_CTL_ADD,
        socket_fd,
        &mut event
    ))?;
    Ok(())
}

// 读事件标志, ONESHOT表示仅通知一次,之后删除.之后需要重新注册需要的事件类型
const READ_FLAGS: i32 = libc::EPOLLONESHOT | libc::EPOLLIN;
const WRITE_FLAGS: i32 = libc::EPOLLONESHOT | libc::EPOLLOUT;

// 创建读监听事件
fn listener_read_event(key: u64) -> libc::epoll_event {
    libc::epoll_event {
        events: READ_FLAGS as u32,
        u64: key,
    }
}

fn listener_write_event(key: u64) -> libc::epoll_event {
    libc::epoll_event {
        events: WRITE_FLAGS as u32,
        u64: key,
    }
}

// 修改监听事件
fn modify_interest(
    epoll_fd: RawFd,
    socket_fd: RawFd,
    mut event: libc::epoll_event,
) -> io::Result<()> {
    syscall!(epoll_ctl(
        epoll_fd,
        libc::EPOLL_CTL_MOD,
        socket_fd,
        &mut event
    ))?;
    Ok(())
}

// TcpStream and Buffer Wrapper
#[derive(Debug)]
struct RequestContext {
    stream: TcpStream,
    buf: Vec<u8>,
    length: usize,
}

impl RequestContext {
    fn new(stream: TcpStream) -> Self {
        RequestContext {
            stream,
            buf: vec![],
            length: 0,
        }
    }

    fn read_cb(&mut self, key: u64, epoll_fd: RawFd) -> io::Result<()> {
        let mut buf = [0u8; 1024];
        match self.stream.read(&mut buf) {
            Ok(_) => {
                if let Ok(data) = std::str::from_utf8(&buf) {
                    self.parse_and_set_content_length(data);
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
        self.buf.extend_from_slice(&buf);
        // 此次http请求数据已经读取完成
        if self.buf.len() >= self.length {
            println!("Got all data: {} bytes", self.length);
            // 修改为返回http请求的事件
            modify_interest(epoll_fd, self.stream.as_raw_fd(), listener_write_event(key))?;
        } else {
            // 数据未读取完成,继续读
            modify_interest(epoll_fd, self.stream.as_raw_fd(), listener_read_event(key))?;
        }
        Ok(())
    }

    fn write_cb(&mut self, key: u64, epoll_fd: RawFd) -> io::Result<()> {
        // 回写http
        match self.stream.write(HTTP_RESP) {
            Ok(_) => println!("answered from request: {}", key),
            Err(e) => eprintln!("Could not answer to key : {} for {}", key, e),
        }
        // 关闭连接
        self.stream.shutdown(std::net::Shutdown::Both)?;
        // 移除监听队列
        let fd = self.stream.as_raw_fd();
        remove_interest(epoll_fd, fd)?;
        close(fd);
        Ok(())
    }

    fn parse_and_set_content_length(&mut self, data: &str) {
        if data.contains("HTTP") {
            if let Some(content_length) = data
                .lines()
                .find(|l| l.to_lowercase().starts_with("content-length: "))
            {
                if let Some(len) = content_length
                    .to_lowercase()
                    .strip_prefix("content-length: ")
                {
                    self.length = len.parse::<usize>().expect("content-length is valid");
                    println!("set content length: {} bytes", self.length);
                }
            }
        }
    }
}

// 移除事件监听
fn remove_interest(epoll_fd: RawFd, socket_fd: RawFd) -> io::Result<()> {
    syscall!(epoll_ctl(
        epoll_fd,
        libc::EPOLL_CTL_DEL,
        socket_fd,
        std::ptr::null_mut()
    ))?;
    Ok(())
}

fn close(fd: RawFd) {
    let _ = syscall!(close(fd));
}
