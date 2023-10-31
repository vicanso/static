package main

import (
	"bytes"
	"fmt"
	"os"
	"regexp"
	"strconv"
	"time"

	"log"

	"github.com/vicanso/elton"
	"github.com/vicanso/elton/middleware"
)

func main() {
	staticPath := os.Getenv("STATIC_PATH")
	compressLevel, _ := strconv.Atoi(os.Getenv("STATIC_COMPRESS_LEVEL"))
	minLength, _ := strconv.Atoi(os.Getenv("STATIC_COMPRESS_MIN_LENGTH"))
	var checker *regexp.Regexp
	contentType := os.Getenv("STATIC_COMPRESS_CONTENT_TYPE")
	if contentType != "" {
		checker, _ = regexp.Compile(contentType)
	}
	cacheTTL, _ := time.ParseDuration(os.Getenv("STATIC_CACHE_TTL"))
	if cacheTTL == 0 {
		cacheTTL = 10 * time.Minute
	}
	disabledLog := os.Getenv("STATIC_DISABLE_LOG") != ""
	e := elton.New()

	if !disabledLog {
		e.Use(middleware.NewLogger(middleware.LoggerConfig{
			OnLog: func(s string, _ *elton.Context) {
				log.Println(s)
			},
			Format: `{remote} {when-iso} "{method} {uri} {proto}" {status} {size-human} - {latency-ms}ms "{referer}" "{userAgent}"`,
		}))
	}
	e.Use(middleware.NewDefaultFresh())

	var compressor middleware.CacheCompressor
	// 基本全部浏览器均支持br
	if compressLevel != 0 {
		compressor = &middleware.CacheBrCompressor{
			Level:         compressLevel,
			MinLength:     minLength,
			ContentRegexp: checker,
		}
	}

	e.Use(middleware.NewCache(middleware.CacheConfig{
		Store:      middleware.NewPeekLruStore(256),
		Compressor: compressor,
	}))

	sf := new(middleware.FS)
	e.GET("/ping", func(c *elton.Context) error {
		c.BodyBuffer = bytes.NewBufferString("pong")
		return nil
	})
	e.GET("/", func(c *elton.Context) (err error) {
		r, err := sf.NewReader(staticPath + "/index.html")
		if err != nil {
			return
		}
		c.NoCache()
		c.SetContentTypeByExt(".html")
		c.Body = r
		return
	})
	// static file route
	e.GET("/*", middleware.NewStaticServe(sf, middleware.StaticServeConfig{
		Path: staticPath,
		// 客户端缓存一年
		MaxAge: 365 * 24 * time.Hour,
		// 缓存服务器缓存一个小时
		SMaxAge: time.Hour,
		// 禁止访问隐藏文件
		DenyDot: true,
		// 启用强ETag
		EnableStrongETag: true,
		NoCacheRegexp:    regexp.MustCompile(`.html`),
		IndexFile:        "index.html",
	}))
	msg := fmt.Sprintf("path:%s, compress(level:%d, minLength:%d, contentType:%s)", staticPath, compressLevel, minLength, contentType)
	log.Println(msg)
	log.Println("server is running, http://127.0.0.1:3000")

	err := e.ListenAndServe(":3000")
	if err != nil {
		panic(err)
	}
}
